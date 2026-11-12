//! FastRDP video datagram layer: splits encoded frames into MTU-sized shards, adds Reed-Solomon
//! parity per block, and reassembles/recovers on the receiver. Transport-agnostic (QUIC datagrams
//! today, raw UDP possible later).
//!
//! Datagram = 28-byte header + shard payload (all shards of a block have the same length; the last
//! data shard of the frame is zero-padded, `frame_bytes` restores the exact size).
//!
//! ```text
//! 0  u8  version (1)          1  u8  flags (bit0 keyframe, bit1 parity shard)
//! 2  u16 block_index          4  u16 block_count
//! 6  u8  shard_index          7  u8  data_shards (k)       8 u8 parity_shards (m)
//! 9  u8  encoder              10 u16 shard_len
//! 12 u32 frame_id             16 u32 frame_bytes          20 i64 pts_us (capture QPC, us)
//! ```

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use reed_solomon_erasure::galois_8::ReedSolomon;

pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 28;
pub const FLAG_KEYFRAME: u8 = 1;
pub const FLAG_PARITY: u8 = 2;
/// Data shards per FEC block (k <= 64 keeps RS cheap; frames larger than k shards use several blocks).
pub const MAX_DATA_SHARDS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShardHeader {
    pub flags: u8,
    pub block_index: u16,
    pub block_count: u16,
    pub shard_index: u8,
    pub data_shards: u8,
    pub parity_shards: u8,
    pub encoder: u8,
    pub shard_len: u16,
    pub frame_id: u32,
    pub frame_bytes: u32,
    pub pts_us: i64,
}

impl ShardHeader {
    pub fn write(&self, out: &mut Vec<u8>) {
        out.push(VERSION);
        out.push(self.flags);
        out.extend_from_slice(&self.block_index.to_le_bytes());
        out.extend_from_slice(&self.block_count.to_le_bytes());
        out.push(self.shard_index);
        out.push(self.data_shards);
        out.push(self.parity_shards);
        out.push(self.encoder);
        out.extend_from_slice(&self.shard_len.to_le_bytes());
        out.extend_from_slice(&self.frame_id.to_le_bytes());
        out.extend_from_slice(&self.frame_bytes.to_le_bytes());
        out.extend_from_slice(&self.pts_us.to_le_bytes());
    }

    pub fn parse(b: &[u8]) -> Option<(ShardHeader, &[u8])> {
        if b.len() < HEADER_LEN || b[0] != VERSION {
            return None;
        }
        let u16_at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32_at = |i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap());
        let h = ShardHeader {
            flags: b[1],
            block_index: u16_at(2),
            block_count: u16_at(4),
            shard_index: b[6],
            data_shards: b[7],
            parity_shards: b[8],
            encoder: b[9],
            shard_len: u16_at(10),
            frame_id: u32_at(12),
            frame_bytes: u32_at(16),
            pts_us: i64::from_le_bytes(b[20..28].try_into().unwrap()),
        };
        let payload = &b[HEADER_LEN..];
        let total = h.data_shards as usize + h.parity_shards as usize;
        if h.data_shards == 0
            || total > 255
            || h.block_count == 0
            || h.block_index >= h.block_count
            || h.shard_index as usize >= total
            || payload.len() != h.shard_len as usize
        {
            return None;
        }
        Some((h, payload))
    }
}

/// Sender side.
pub struct Packetizer {
    /// Max datagram size accepted by the transport (QUIC: `Connection::max_datagram_size`).
    pub max_datagram: usize,
    /// Parity ratio, e.g. 0.20 = 20 % extra shards (rounded up, min 1 per block when > 0).
    pub fec_ratio: f32,
    /// Frame ids must stay continuous for the whole connection (even when the encoder / session
    /// changes), otherwise the receiver treats a restarted sequence as "old" and drops it.
    next_frame_id: Arc<AtomicU32>,
    codecs: HashMap<(usize, usize), ReedSolomon>,
}

impl Packetizer {
    pub fn new(max_datagram: usize, fec_ratio: f32) -> Self {
        Self::with_id_source(max_datagram, fec_ratio, Arc::new(AtomicU32::new(1)))
    }

    /// Shares one frame-id counter across packetizers of the same connection.
    pub fn with_id_source(max_datagram: usize, fec_ratio: f32, ids: Arc<AtomicU32>) -> Self {
        Packetizer { max_datagram, fec_ratio, next_frame_id: ids, codecs: HashMap::new() }
    }

    pub fn shard_len(&self) -> usize {
        self.max_datagram.saturating_sub(HEADER_LEN).max(64)
    }

    /// Splits one encoded frame into datagrams (data shards first, then parity, block by block).
    pub fn packetize(&mut self, frame: &[u8], keyframe: bool, encoder: u8, pts_us: i64) -> (u32, Vec<Vec<u8>>) {
        let mut frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        if frame_id == 0 {
            frame_id = self.next_frame_id.fetch_add(1, Ordering::Relaxed);
        }
        let s = self.shard_len();
        let total_data = frame.len().div_ceil(s).max(1);
        let block_count = total_data.div_ceil(MAX_DATA_SHARDS);
        let mut out = Vec::new();
        for b in 0..block_count {
            let first = b * MAX_DATA_SHARDS;
            let k = (total_data - first).min(MAX_DATA_SHARDS);
            let m = if self.fec_ratio <= 0.0 { 0 } else { ((k as f32 * self.fec_ratio).ceil() as usize).clamp(1, 255 - k) };
            let mut shards: Vec<Vec<u8>> = (0..k + m)
                .map(|i| {
                    let mut v = vec![0u8; s];
                    if i < k {
                        let start = (first + i) * s;
                        let end = (start + s).min(frame.len());
                        if start < frame.len() {
                            v[..end - start].copy_from_slice(&frame[start..end]);
                        }
                    }
                    v
                })
                .collect();
            if m > 0 {
                let rs = self.codecs.entry((k, m)).or_insert_with(|| ReedSolomon::new(k, m).expect("rs params"));
                rs.encode(&mut shards).expect("rs encode");
            }
            for (i, sh) in shards.iter().enumerate() {
                let h = ShardHeader {
                    flags: (keyframe as u8) | if i >= k { FLAG_PARITY } else { 0 },
                    block_index: b as u16,
                    block_count: block_count as u16,
                    shard_index: i as u8,
                    data_shards: k as u8,
                    parity_shards: m as u8,
                    encoder,
                    shard_len: s as u16,
                    frame_id,
                    frame_bytes: frame.len() as u32,
                    pts_us,
                };
                let mut d = Vec::with_capacity(HEADER_LEN + s);
                h.write(&mut d);
                d.extend_from_slice(sh);
                out.push(d);
            }
        }
        (frame_id, out)
    }
}

#[derive(Clone, Debug)]
pub struct CompleteFrame {
    pub frame_id: u32,
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub encoder: u8,
    pub pts_us: i64,
    /// true if at least one data shard was rebuilt from parity
    pub recovered: bool,
}

#[derive(Default, Clone, Debug)]
pub struct ReassemblyStats {
    pub datagrams: u64,
    pub malformed: u64,
    pub frames_complete: u64,
    pub frames_recovered: u64,
    pub frames_lost: u64,
    pub late_datagrams: u64,
}

struct Block {
    k: usize,
    m: usize,
    shards: Vec<Option<Vec<u8>>>,
    have: usize,
    done: Option<(Vec<u8>, bool)>,
}

struct Pending {
    first_seen: Instant,
    blocks: Vec<Option<Block>>,
    frame_bytes: usize,
    keyframe: bool,
    encoder: u8,
    pts_us: i64,
}

/// Receiver side. Frames are delivered in frame_id order; a frame still incomplete when a newer
/// frame completes, or after `timeout`, is declared lost (the caller then asks for an IDR).
pub struct Reassembler {
    pending: BTreeMap<u32, Pending>,
    last_delivered: Option<u32>,
    pub timeout: Duration,
    pub stats: ReassemblyStats,
    codecs: HashMap<(usize, usize), ReedSolomon>,
}

impl Default for Reassembler {
    fn default() -> Self {
        Self::new(Duration::from_millis(100))
    }
}

fn newer(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) > 0
}

impl Reassembler {
    pub fn new(timeout: Duration) -> Self {
        Reassembler { pending: BTreeMap::new(), last_delivered: None, timeout, stats: Default::default(), codecs: HashMap::new() }
    }

    /// Feeds one datagram. Returns frames that became complete (in order) and the ids of frames
    /// given up as lost by this call.
    pub fn push(&mut self, datagram: &[u8], now: Instant) -> (Vec<CompleteFrame>, Vec<u32>) {
        self.stats.datagrams += 1;
        let Some((h, payload)) = ShardHeader::parse(datagram) else {
            self.stats.malformed += 1;
            return (vec![], vec![]);
        };
        if let Some(last) = self.last_delivered
            && !newer(h.frame_id, last)
        {
            self.stats.late_datagrams += 1;
            return (vec![], vec![]);
        }
        let p = self.pending.entry(h.frame_id).or_insert_with(|| Pending {
            first_seen: now,
            blocks: (0..h.block_count).map(|_| None).collect(),
            frame_bytes: h.frame_bytes as usize,
            keyframe: h.flags & FLAG_KEYFRAME != 0,
            encoder: h.encoder,
            pts_us: h.pts_us,
        });
        if (h.block_index as usize) < p.blocks.len() {
            let k = h.data_shards as usize;
            let m = h.parity_shards as usize;
            let blk = p.blocks[h.block_index as usize].get_or_insert_with(|| Block { k, m, shards: vec![None; k + m], have: 0, done: None });
            if blk.done.is_none() && blk.k == k && blk.m == m && blk.shards[h.shard_index as usize].is_none() {
                blk.shards[h.shard_index as usize] = Some(payload.to_vec());
                blk.have += 1;
                if blk.have >= k {
                    let missing_data = blk.shards[..k].iter().any(|s| s.is_none());
                    if missing_data {
                        let rs = self.codecs.entry((k, m)).or_insert_with(|| ReedSolomon::new(k, m).expect("rs"));
                        if rs.reconstruct_data(&mut blk.shards).is_ok() {
                            let data: Vec<u8> = blk.shards[..k].iter().flat_map(|s| s.as_ref().unwrap().iter().copied()).collect();
                            blk.done = Some((data, true));
                        }
                    } else {
                        let data: Vec<u8> = blk.shards[..k].iter().flat_map(|s| s.as_ref().unwrap().iter().copied()).collect();
                        blk.done = Some((data, false));
                    }
                    if blk.done.is_some() {
                        blk.shards = Vec::new();
                    }
                }
            }
        }
        self.drain(now)
    }

    /// Expires old frames (call periodically even without traffic).
    pub fn poll(&mut self, now: Instant) -> Vec<u32> {
        self.drain(now).1
    }

    fn drain(&mut self, now: Instant) -> (Vec<CompleteFrame>, Vec<u32>) {
        let mut out = Vec::new();
        let mut lost = Vec::new();
        // newest complete frame id (frames older than it that are incomplete are lost)
        let newest_complete =
            self.pending.iter().filter(|(_, p)| p.blocks.iter().all(|b| b.as_ref().is_some_and(|b| b.done.is_some()))).map(|(id, _)| *id).max();
        let ids: Vec<u32> = self.pending.keys().copied().collect();
        for id in ids {
            let complete = self.pending[&id].blocks.iter().all(|b| b.as_ref().is_some_and(|b| b.done.is_some()));
            if complete {
                let p = self.pending.remove(&id).unwrap();
                let mut data = Vec::with_capacity(p.frame_bytes);
                let mut recovered = false;
                for b in p.blocks.into_iter().flatten() {
                    let (d, r) = b.done.unwrap();
                    recovered |= r;
                    data.extend_from_slice(&d);
                }
                data.truncate(p.frame_bytes);
                self.stats.frames_complete += 1;
                self.stats.frames_recovered += recovered as u64;
                self.last_delivered = Some(id);
                out.push(CompleteFrame { frame_id: id, data, keyframe: p.keyframe, encoder: p.encoder, pts_us: p.pts_us, recovered });
                continue;
            }
            let expired = now.duration_since(self.pending[&id].first_seen) > self.timeout;
            let superseded = newest_complete.is_some_and(|n| newer(n, id));
            if expired || superseded {
                self.pending.remove(&id);
                self.stats.frames_lost += 1;
                lost.push(id);
                if self.last_delivered.is_none_or(|l| newer(id, l)) {
                    self.last_delivered = Some(id);
                }
            } else {
                break; // keep order: wait for this frame before delivering newer ones
            }
        }
        (out, lost)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(n: usize, seed: u8) -> Vec<u8> {
        (0..n).map(|i| (i as u8).wrapping_mul(31).wrapping_add(seed)).collect()
    }

    #[test]
    fn roundtrip_no_loss_multi_block() {
        let mut p = Packetizer::new(1200, 0.2);
        let mut r = Reassembler::default();
        let f = frame(300_000, 7); // ~255 shards -> 4 blocks
        let (_, dgs) = p.packetize(&f, true, 0, 42);
        let now = Instant::now();
        let mut got = vec![];
        for d in &dgs {
            assert!(d.len() <= 1200);
            got.extend(r.push(d, now).0);
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, f);
        assert!(got[0].keyframe && !got[0].recovered);
        assert_eq!(got[0].pts_us, 42);
    }

    #[test]
    fn recovers_from_loss_within_parity() {
        let mut p = Packetizer::new(1200, 0.25);
        let mut r = Reassembler::default();
        let f = frame(50_000, 3); // 43 shards, 11 parity
        let (_, dgs) = p.packetize(&f, false, 0, 0);
        let now = Instant::now();
        let mut got = vec![];
        for (i, d) in dgs.iter().enumerate() {
            if i % 5 == 1 {
                continue; // drop 20 %
            }
            got.extend(r.push(d, now).0);
        }
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].data, f);
        assert!(got[0].recovered);
    }

    #[test]
    fn too_much_loss_is_reported_when_newer_frame_completes() {
        let mut p = Packetizer::new(1200, 0.1);
        let mut r = Reassembler::default();
        let now = Instant::now();
        let (id1, d1) = p.packetize(&frame(20_000, 1), false, 0, 0);
        let (_, d2) = p.packetize(&frame(2_000, 2), false, 0, 0);
        for d in d1.iter().step_by(2) {
            r.push(d, now);
        }
        let mut lost = vec![];
        let mut got = vec![];
        for d in &d2 {
            let (g, l) = r.push(d, now);
            got.extend(g);
            lost.extend(l);
        }
        assert_eq!(lost, vec![id1]);
        assert_eq!(got.len(), 1);
        assert_eq!(r.stats.frames_lost, 1);
    }

    #[test]
    fn timeout_expires_incomplete_frame() {
        let mut p = Packetizer::new(1200, 0.0);
        let mut r = Reassembler::new(Duration::from_millis(50));
        let t0 = Instant::now();
        let (id, d) = p.packetize(&frame(10_000, 9), false, 0, 0);
        r.push(&d[0], t0);
        assert!(r.poll(t0 + Duration::from_millis(10)).is_empty());
        assert_eq!(r.poll(t0 + Duration::from_millis(60)), vec![id]);
    }

    #[test]
    fn malformed_and_late_are_counted() {
        let mut p = Packetizer::new(1200, 0.0);
        let mut r = Reassembler::default();
        let now = Instant::now();
        r.push(&[1, 2, 3], now);
        let (_, d) = p.packetize(&frame(100, 0), false, 0, 0);
        r.push(&d[0], now);
        r.push(&d[0], now); // duplicate of an already delivered frame
        assert_eq!(r.stats.malformed, 1);
        assert_eq!(r.stats.late_datagrams, 1);
        assert_eq!(r.stats.frames_complete, 1);
    }
}
