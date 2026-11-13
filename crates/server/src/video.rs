//! Per-client video loop (blocking thread): session frame shared memory -> AdaptiveEncoder
//! (configured encoder, OpenH264 fallback) -> FEC packetizer -> QUIC datagrams.
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use encoder::{AdaptiveEncoder, EncoderKind, EncoderSettings, Frame, PixelFormat};
use frame_shm::FrameShm;
use protocol::{CursorPos, CursorShape, Event, EventMsg, Message, StreamInfo};
use tokio::sync::mpsc::UnboundedSender;
use transport::Packetizer;

use crate::{info, warn};

/// Shared between the control-stream reader (client feedback) and the video thread.
#[derive(Default)]
pub struct Feedback {
    pub idr_requested: AtomicBool,
    /// last reported loss in 1/1000 (frames_lost / frames_total)
    pub loss_permille: AtomicU32,
}

pub struct VideoParams {
    pub frame_ids: Arc<std::sync::atomic::AtomicU32>,
    /// current desktop size (w << 32 | h), read by the input relay to normalize coordinates
    pub desktop: Arc<std::sync::atomic::AtomicU64>,
    pub session: u32,
    pub fec_base: f32,
    /// refresh rate the client asked for (Hz): encoder frame rate and bitrate follow it
    pub refresh_hz: Arc<AtomicU32>,
    /// bitrate target the client asked for (kbit/s, 0 = automatic)
    pub bitrate_kbps: Arc<AtomicU32>,
    /// pointer shape ids on the wire, unique per connection (see CursorRelay)
    pub cursor_ids: Arc<AtomicU32>,
}

pub fn encoder_code(k: EncoderKind) -> u8 {
    match k {
        EncoderKind::OpenH264 => 1,
        EncoderKind::Nvenc => 2,
        EncoderKind::MediaFoundation => 3,
    }
}

fn stream_info(e: &AdaptiveEncoder, w: u32, h: u32, fps: u32) -> Message {
    Message::StreamInfo(StreamInfo { width: w, height: h, fps, encoder: encoder_code(e.active()), fallback: e.is_fallback(), description: e.describe() })
}

/// Forwards the driver's hardware-cursor state: the shape when it changes, the hotspot position
/// coalesced to at most one message per `MIN_POS_INTERVAL`.
///
/// Windows numbers pointer shapes per monitor, so after a session switch (sign-in screen -> the
/// user's session) or a new display the same id stands for another shape; the client caches its
/// cursors by id and kept showing the old one. Shapes therefore go out with ids of this connection.
struct CursorRelay {
    /// the driver's id of the current shape
    shape_id: u32,
    /// its id on the wire
    wire_id: u32,
    ids: Arc<AtomicU32>,
    hot: (i32, i32),
    last_sent: Option<CursorPos>,
    sent_at: Instant,
    pending: bool,
    shapes: u64,
}

const MIN_POS_INTERVAL: Duration = Duration::from_millis(8);

impl CursorRelay {
    fn new(ids: Arc<AtomicU32>) -> Self {
        CursorRelay { shape_id: 0, wire_id: 0, ids, hot: (0, 0), last_sent: None, sent_at: Instant::now(), pending: false, shapes: 0 }
    }

    /// Returns true while a position update is waiting for the rate limit.
    fn poll(&mut self, shm: &FrameShm, ctrl: &UnboundedSender<Message>) -> bool {
        let Some((st, shape)) = shm.read_cursor(self.shape_id) else { return self.pending };
        if let Some(s) = shape {
            self.shape_id = s.id;
            self.wire_id = self.ids.fetch_add(1, Ordering::Relaxed);
            self.hot = (s.x_hot as i32, s.y_hot as i32);
            self.shapes += 1;
            let rgba = protocol::cursor::rgba_from_bgra(s.shape_type, &s.bgra);
            let _ = ctrl.send(Message::CursorShape(CursorShape {
                id: self.wire_id,
                width: s.width as u16,
                height: s.height as u16,
                hot_x: s.x_hot as u16,
                hot_y: s.y_hot as u16,
                rgba,
            }));
        }
        let pos = CursorPos { visible: st.visible, x: st.x + self.hot.0, y: st.y + self.hot.1, shape_id: self.wire_id };
        if self.last_sent == Some(pos) {
            self.pending = false;
            return false;
        }
        if self.sent_at.elapsed() < MIN_POS_INTERVAL && self.last_sent.is_some_and(|l| l.visible == pos.visible && l.shape_id == pos.shape_id) {
            self.pending = true;
            return true;
        }
        let _ = ctrl.send(Message::CursorPos(pos));
        self.last_sent = Some(pos);
        self.sent_at = Instant::now();
        self.pending = false;
        false
    }
}

/// Bitrate control: the configured `bitrate_kbps` is for 1920x1080; other sizes scale with
/// pixels^0.8. Reported frame loss lowers it (x0.75, at most every 2 s, floor 25 %); 6 s without
/// loss raise it again (x1.15) up to the target.
pub struct RateControl {
    base_kbps: u32,
    /// target chosen by the client, instead of the automatic one
    fixed: Option<u32>,
    target: u32,
    current: u32,
    last_change: Instant,
    clean_since: Instant,
}

impl RateControl {
    pub fn new(base_kbps: u32) -> Self {
        let now = Instant::now();
        RateControl { base_kbps, fixed: None, target: base_kbps, current: base_kbps, last_change: now, clean_since: now }
    }

    /// `base_kbps` is for 1920x1080 at 60 Hz. More pixels need more bits (exponent 0.8); more frames
    /// per second need more bits too, but each frame differs less from the previous one (0.5).
    pub fn target_for(base_kbps: u32, w: u32, h: u32, fps: u32) -> u32 {
        let ratio = (w as f64 * h as f64) / (1920.0 * 1080.0);
        let rate = f64::from(fps.max(1)) / 60.0;
        ((base_kbps as f64 * ratio.powf(0.8) * rate.powf(0.5)).round() as u32).clamp(2_000, 120_000)
    }

    /// Target chosen by the client (kbit/s, already range-checked), or 0 = automatic. Takes effect
    /// with the next `resize`.
    pub fn set_fixed(&mut self, kbps: u32) {
        self.fixed = (kbps > 0).then_some(kbps);
    }

    /// New desktop size, frame rate or client target: start at the full target for it.
    pub fn resize(&mut self, w: u32, h: u32, fps: u32) -> u32 {
        self.target = self.fixed.unwrap_or_else(|| Self::target_for(self.base_kbps, w, h, fps));
        self.current = self.target;
        self.current
    }

    /// Called about once per second with the loss reported by the client (per mille).
    /// Returns the new bitrate when it changed.
    pub fn update(&mut self, loss_permille: u32, now: Instant) -> Option<u32> {
        if loss_permille > 30 {
            self.clean_since = now;
            if now.duration_since(self.last_change) >= Duration::from_secs(2) && self.current > self.target / 4 {
                self.current = (self.current * 3 / 4).max(self.target / 4);
                self.last_change = now;
                return Some(self.current);
            }
        } else if loss_permille <= 5 {
            if self.current < self.target
                && now.duration_since(self.clean_since) >= Duration::from_secs(6)
                && now.duration_since(self.last_change) >= Duration::from_secs(2)
            {
                self.current = (self.current * 115 / 100).min(self.target);
                self.last_change = now;
                return Some(self.current);
            }
        } else {
            self.clean_since = now;
        }
        None
    }
}

pub fn run(conn: quinn::Connection, p: VideoParams, ctrl: UnboundedSender<Message>, fb: Arc<Feedback>, stop: Arc<AtomicBool>) {
    let tag = format!("video[{} s{}]", conn.remote_address(), p.session);
    let t0 = Instant::now();
    let shm = loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        match FrameShm::open(p.session, true) {
            Ok(s) => break s,
            Err(e) if t0.elapsed() < Duration::from_secs(30) => {
                if t0.elapsed() < Duration::from_millis(250) {
                    info!("{tag}: waiting for frame shared memory ({e})");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            Err(e) => {
                warn!("{tag}: frame shared memory not available after 30 s: {e}");
                let _ = ctrl.send(Message::Event(EventMsg { event: Event::Error as u32, value: 0, text: format!("no frame source: {e}") }));
                return;
            }
        }
    };
    let settings = match EncoderSettings::load(&EncoderSettings::default_path()) {
        Ok(s) => s,
        Err(e) => {
            warn!("{tag}: encoder.toml invalid ({e}) - using defaults (OpenH264)");
            EncoderSettings::default()
        }
    };
    info!("{tag}: frame source open (producer pid {}), requested encoder {}", shm.header().producer_pid, settings.encoder);

    let mut enc: Option<AdaptiveEncoder> = None;
    let mut rate = RateControl::new(settings.bitrate_kbps);
    let mut rate_tick = Instant::now();
    let mut enc_size: Option<(u32, u32)> = None;
    let mut enc_fps = 0u32;
    let mut wanted_kbps = 0u32;
    let mut pk = Packetizer::with_id_source(conn.max_datagram_size().unwrap_or(1200), p.fec_base, p.frame_ids.clone());
    let mut buf = Vec::new();
    let mut last_frame = -1i64;
    let mut force = true;
    let mut cursor = CursorRelay::new(p.cursor_ids.clone());
    let mut cursor_pending = false;
    while !stop.load(Ordering::SeqCst) {
        shm.wait(if cursor_pending { 4 } else { 250 });
        cursor_pending = cursor.poll(&shm, &ctrl);
        if fb.idr_requested.swap(false, Ordering::SeqCst) {
            if let Some(e) = enc.as_mut() {
                e.request_idr();
            }
            force = true;
        }
        let latest = shm.latest_frame();
        if latest < 0 || (latest == last_frame && !force) {
            continue;
        }
        let Some(meta) = shm.read_latest(&mut buf) else { continue };
        last_frame = meta.frame_number as i64;
        force = false;
        let (w, h) = (meta.width, meta.height);
        p.desktop.store((w as u64) << 32 | h as u64, Ordering::Relaxed);
        if enc_size != Some((w, h)) {
            if let Some((ow, oh)) = enc_size {
                info!("{tag}: desktop resized {ow}x{oh} -> {w}x{h}: new encoder");
            }
            enc = None;
            enc_size = Some((w, h));
        }
        // the client's refresh rate (24..=240, validated by DisplayRequest::clamped)
        let hz = p.refresh_hz.load(Ordering::Relaxed).clamp(24, 240);
        if hz != enc_fps {
            if enc.is_some() {
                info!("{tag}: refresh rate {enc_fps} -> {hz} Hz: new encoder");
            }
            enc = None;
            enc_fps = hz;
        }
        // the client's bitrate target (0 = automatic); loss still lowers it temporarily
        let want = p.bitrate_kbps.load(Ordering::Relaxed);
        if want != wanted_kbps {
            wanted_kbps = want;
            rate.set_fixed(want);
            if let Some(e) = enc.as_mut() {
                let kbps = rate.resize(w, h, enc_fps);
                info!("{tag}: bitrate target {} -> {kbps} kbps", if want == 0 { "automatic" } else { "client" });
                if let Err(err) = e.set_bitrate(kbps) {
                    warn!("{tag}: set_bitrate: {err}");
                }
            }
        }
        if enc.as_ref().is_none() {
            let mut s = settings.clone();
            s.fps = enc_fps;
            s.bitrate_kbps = rate.resize(w, h, enc_fps);
            match AdaptiveEncoder::new(s, w, h) {
                Ok(mut e) => {
                    info!("{tag}: encoder {} ({})", e.active(), e.describe());
                    for ev in e.take_events() {
                        let text = format!("{} -> {}: {}", ev.from, ev.to, ev.reason);
                        warn!("{tag}: FALLBACK {:?} {text}", ev.phase);
                        let _ = ctrl.send(Message::Event(EventMsg { event: Event::EncoderFallback as u32, value: 1, text }));
                    }
                    let _ = ctrl.send(stream_info(&e, w, h, enc_fps));
                    enc = Some(e);
                }
                Err(e) => {
                    warn!("{tag}: no encoder (OpenH264 fallback failed too): {e}");
                    return;
                }
            }
        }
        let e = enc.as_mut().unwrap();
        if rate_tick.elapsed() >= Duration::from_secs(1) {
            rate_tick = Instant::now();
            let loss = fb.loss_permille.load(Ordering::Relaxed);
            if let Some(kbps) = rate.update(loss, rate_tick) {
                info!("{tag}: bitrate -> {kbps} kbps (frame loss {loss} per mille)");
                if let Err(err) = e.set_bitrate(kbps) {
                    warn!("{tag}: set_bitrate: {err}");
                }
            }
        }
        let format = if meta.format == frame_shm::FORMAT_I420 { PixelFormat::I420 } else { PixelFormat::Bgra };
        let f = Frame { width: w, height: h, stride: meta.stride as usize, format, data: &buf, pts_us: meta.pts_us };
        let packet = match e.encode(&f) {
            Ok(p) => p,
            Err(err) => {
                warn!("{tag}: encode failed: {err}");
                continue;
            }
        };
        let events = e.take_events();
        if !events.is_empty() {
            for ev in events {
                let text = format!("{} -> {}: {}", ev.from, ev.to, ev.reason);
                warn!("{tag}: FALLBACK {:?} {text}", ev.phase);
                let _ = ctrl.send(Message::Event(EventMsg { event: Event::EncoderFallback as u32, value: e.is_fallback() as u32, text }));
            }
            let _ = ctrl.send(stream_info(e, w, h, enc_fps));
        }
        let Some(pkt) = packet else { continue };
        let loss = fb.loss_permille.load(Ordering::Relaxed) as f32 / 1000.0;
        pk.fec_ratio = (p.fec_base + 2.0 * loss).clamp(p.fec_base, 0.5);
        if let Some(m) = conn.max_datagram_size() {
            pk.max_datagram = m;
        }
        let (_, dgs) = pk.packetize(&pkt.data, pkt.keyframe, encoder_code(pkt.encoder), meta.pts_us);
        for d in dgs {
            if let Err(err) = conn.send_datagram(Bytes::from(d)) {
                warn!("{tag}: send_datagram: {err} - stopping");
                return;
            }
        }
    }
    info!("{tag}: stopped ({} cursor shape(s) sent)", cursor.shapes);
}

#[cfg(test)]
mod tests {
    use super::RateControl;
    use std::time::{Duration, Instant};

    #[test]
    fn bitrate_scales_with_resolution() {
        assert_eq!(RateControl::target_for(15_000, 1920, 1080, 60), 15_000);
        let hfr = RateControl::target_for(15_000, 1920, 1080, 240);
        assert_eq!(hfr, 30_000, "4x the frames: 2x the bits");
        let uhd = RateControl::target_for(15_000, 3840, 2160, 60);
        assert!(uhd > 44_000 && uhd < 46_000, "{uhd}");
        assert!(RateControl::target_for(15_000, 1280, 720, 60) < 10_000);
        assert_eq!(RateControl::target_for(1_000, 640, 480, 60), 2_000, "floor");
    }

    #[test]
    fn client_target_replaces_the_automatic_one() {
        let mut r = RateControl::new(15_000);
        r.set_fixed(40_000);
        assert_eq!(r.resize(1920, 1080, 60), 40_000);
        r.set_fixed(0);
        assert_eq!(r.resize(1920, 1080, 60), 15_000, "back to automatic");
    }

    #[test]
    fn loss_lowers_and_clean_period_restores() {
        let t0 = Instant::now() + Duration::from_secs(100);
        let mut r = RateControl::new(15_000);
        r.resize(1920, 1080, 60);
        assert_eq!(r.update(80, t0), Some(11_250));
        assert_eq!(r.update(80, t0 + Duration::from_secs(1)), None, "at most every 2 s");
        let mut t = t0;
        for _ in 0..20 {
            t += Duration::from_secs(2);
            r.update(80, t);
        }
        assert_eq!(r.current, 3_750, "floor = 25 %");
        let mut up = 0;
        for i in 1..=60 {
            if r.update(0, t + Duration::from_secs(i)).is_some() {
                up += 1;
            }
        }
        assert!(up > 0 && r.current == 15_000, "restored to target ({}, {up} steps)", r.current);
    }
}
