//! FastRDP video encoding with a user-selected encoder and a **mandatory OpenH264 fallback**.
//!
//! Policy (user requirement):
//! * the encoder is user-configurable (`encoder.toml`), **default = OpenH264** (built from source,
//!   BSD); NVENC uses the NVIDIA GPU;
//! * if the configured encoder is missing (no GPU/driver/DLL) or fails to initialise -> OpenH264;
//! * if it fails at runtime (device removed, driver reset, session limit, encode error) -> switch to
//!   OpenH264 immediately, re-encode the same frame as an IDR so the stream stays decodable;
//! * optionally re-try the configured encoder every `retry_requested_secs`.
//!
//! Every switch is recorded as a [`FallbackEvent`] for logs/telemetry/client notification.

pub mod backends;
pub mod convert;
pub mod nvenc;
pub mod settings;

use std::fmt;
use std::time::{Duration, Instant};

pub use settings::{EncoderKind, EncoderSettings};

/// The encoder every other one falls back to (software, always available).
pub const FALLBACK: EncoderKind = EncoderKind::OpenH264;

/// Pixel layout of a [`Frame`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PixelFormat {
    /// B, G, R, A bytes; `stride` bytes per row (the IddCx swap-chain format).
    Bgra,
    /// Planar 4:2:0, BT.709 limited range, tightly packed: Y (width x height), then U and V
    /// (width/2 x height/2 each); `stride` = width. Converted on the GPU by the display driver.
    I420,
}

/// One frame from the display driver.
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row (BGRA) / luma row length (I420).
    pub stride: usize,
    pub format: PixelFormat,
    pub data: &'a [u8],
    pub pts_us: i64,
}

impl Frame<'_> {
    /// Y, U, V planes of an I420 frame (None for BGRA or a buffer that is too small).
    pub fn i420_planes(&self) -> Option<(&[u8], &[u8], &[u8])> {
        if self.format != PixelFormat::I420 {
            return None;
        }
        let (w, h) = (self.width as usize, self.height as usize);
        let (ys, cs) = (w * h, w.div_ceil(2) * h.div_ceil(2));
        let d = self.data.get(..ys + 2 * cs)?;
        let (y, uv) = d.split_at(ys);
        let (u, v) = uv.split_at(cs);
        Some((y, u, v))
    }
}

/// One encoded access unit, H.264 Annex-B.
#[derive(Clone, Debug)]
pub struct Packet {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub pts_us: i64,
    pub encoder: EncoderKind,
}

#[derive(Debug, Clone)]
pub enum EncodeError {
    /// Encoder not present on this machine (no GPU / driver / DLL) or not implemented.
    Unavailable(String),
    InitFailed(String),
    Runtime(String),
    InvalidInput(String),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::Unavailable(s) => write!(f, "unavailable: {s}"),
            EncodeError::InitFailed(s) => write!(f, "init failed: {s}"),
            EncodeError::Runtime(s) => write!(f, "runtime error: {s}"),
            EncodeError::InvalidInput(s) => write!(f, "invalid input: {s}"),
        }
    }
}

impl std::error::Error for EncodeError {}

#[derive(Clone, Debug)]
pub enum FallbackPhase {
    Init,
    Runtime,
    Resize,
}

#[derive(Clone, Debug)]
pub struct FallbackEvent {
    pub phase: FallbackPhase,
    pub from: EncoderKind,
    pub to: EncoderKind,
    pub reason: String,
}

pub struct AdaptiveEncoder {
    settings: EncoderSettings,
    width: u32,
    height: u32,
    active: Box<dyn backends::Backend>,
    events: Vec<FallbackEvent>,
    force_idr: bool,
    fell_back_at: Option<Instant>,
    inject_failures: u32,
}

impl AdaptiveEncoder {
    /// Creates the configured encoder; falls back to OpenH264 when it is missing or fails.
    /// Errors only if OpenH264 itself cannot be created.
    pub fn new(settings: EncoderSettings, width: u32, height: u32) -> Result<Self, EncodeError> {
        settings.validate().map_err(EncodeError::InvalidInput)?;
        let mut events = Vec::new();
        let active = Self::open_with_fallback(&settings, width, height, FallbackPhase::Init, &mut events)?;
        let fell_back_at = (active.kind() != settings.encoder).then(Instant::now);
        Ok(AdaptiveEncoder { settings, width, height, active, events, force_idr: true, fell_back_at, inject_failures: 0 })
    }

    fn open_with_fallback(
        s: &EncoderSettings,
        w: u32,
        h: u32,
        phase: FallbackPhase,
        events: &mut Vec<FallbackEvent>,
    ) -> Result<Box<dyn backends::Backend>, EncodeError> {
        match backends::create(s.encoder, s, w, h) {
            Ok(b) => Ok(b),
            Err(e) if s.encoder != FALLBACK && !matches!(e, EncodeError::InvalidInput(_)) => {
                events.push(FallbackEvent { phase, from: s.encoder, to: FALLBACK, reason: e.to_string() });
                backends::create(FALLBACK, s, w, h)
            }
            Err(e) => Err(e),
        }
    }

    pub fn requested(&self) -> EncoderKind {
        self.settings.encoder
    }
    pub fn active(&self) -> EncoderKind {
        self.active.kind()
    }
    pub fn describe(&self) -> String {
        self.active.describe()
    }
    pub fn is_fallback(&self) -> bool {
        self.active.kind() != self.settings.encoder
    }
    /// Drains recorded fallback/recovery events.
    pub fn take_events(&mut self) -> Vec<FallbackEvent> {
        std::mem::take(&mut self.events)
    }
    /// Current target bitrate.
    pub fn bitrate_kbps(&self) -> u32 {
        self.settings.bitrate_kbps
    }

    /// Changes the target bitrate (in place when the backend supports it, else a new encoder + IDR).
    pub fn set_bitrate(&mut self, kbps: u32) -> Result<(), EncodeError> {
        let kbps = kbps.clamp(500, 500_000);
        if kbps == self.settings.bitrate_kbps {
            return Ok(());
        }
        self.settings.bitrate_kbps = kbps;
        if !self.active.set_bitrate(kbps) {
            let mut ev = Vec::new();
            self.active = Self::open_with_fallback(&self.settings, self.width, self.height, FallbackPhase::Resize, &mut ev)?;
            self.events.extend(ev);
            self.force_idr = true;
        }
        Ok(())
    }

    /// Next frame is encoded as IDR (new client, packet loss recovery...).
    pub fn request_idr(&mut self) {
        self.force_idr = true;
    }

    /// Test hook: makes the next `n` encode calls of the *active* backend fail with a runtime error,
    /// to exercise the runtime fallback path (as if the GPU disappeared).
    #[doc(hidden)]
    pub fn inject_runtime_failures(&mut self, n: u32) {
        self.inject_failures = n;
    }

    pub fn encode(&mut self, f: &Frame<'_>) -> Result<Option<Packet>, EncodeError> {
        if f.width != self.width || f.height != self.height {
            let mut ev = Vec::new();
            self.active = Self::open_with_fallback(&self.settings, f.width, f.height, FallbackPhase::Resize, &mut ev)?;
            self.events.extend(ev);
            self.width = f.width;
            self.height = f.height;
            self.force_idr = true;
        }
        self.maybe_retry_requested();

        let idr = self.force_idr;
        let first = if self.inject_failures > 0 {
            self.inject_failures -= 1;
            Err(EncodeError::Runtime(format!("injected failure in {}", self.active.kind())))
        } else {
            self.active.encode(f, idr)
        };
        match first {
            Ok(p) => {
                if p.is_some() {
                    self.force_idr = false;
                }
                Ok(p)
            }
            Err(e @ EncodeError::InvalidInput(_)) => Err(e),
            Err(e) if self.active.kind() != FALLBACK => {
                let from = self.active.kind();
                self.active = backends::create(FALLBACK, &self.settings, self.width, self.height)?;
                self.events.push(FallbackEvent { phase: FallbackPhase::Runtime, from, to: FALLBACK, reason: e.to_string() });
                self.fell_back_at = Some(Instant::now());
                // Same frame again, as IDR: the decoder must not depend on the dead encoder's references.
                let p = self.active.encode(f, true)?;
                self.force_idr = p.is_none();
                Ok(p)
            }
            Err(e) => {
                // the fallback itself failed: recreate it once, then give up.
                self.active = backends::create(FALLBACK, &self.settings, self.width, self.height)?;
                self.events.push(FallbackEvent {
                    phase: FallbackPhase::Runtime,
                    from: FALLBACK,
                    to: FALLBACK,
                    reason: format!("{FALLBACK} restarted after: {e}"),
                });
                self.active.encode(f, true)
            }
        }
    }

    fn maybe_retry_requested(&mut self) {
        let secs = self.settings.retry_requested_secs;
        let Some(t) = self.fell_back_at else { return };
        if !self.is_fallback() || secs == 0 || t.elapsed() < Duration::from_secs(secs as u64) {
            return;
        }
        self.fell_back_at = Some(Instant::now());
        if let Ok(b) = backends::create(self.settings.encoder, &self.settings, self.width, self.height) {
            self.events.push(FallbackEvent {
                phase: FallbackPhase::Runtime,
                from: FALLBACK,
                to: b.kind(),
                reason: "configured encoder available again".into(),
            });
            self.active = b;
            self.force_idr = true;
            self.fell_back_at = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(buf: &[u8], w: u32, h: u32, pts: i64) -> Frame<'_> {
        Frame { width: w, height: h, stride: w as usize * 4, format: PixelFormat::Bgra, data: buf, pts_us: pts }
    }

    fn pattern(w: usize, h: usize, t: usize) -> Vec<u8> {
        let mut v = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let i = (y * w + x) * 4;
                v[i] = ((x + t) % 256) as u8;
                v[i + 1] = ((y * 2 + t) % 256) as u8;
                v[i + 2] = ((x ^ y) % 256) as u8;
                v[i + 3] = 255;
            }
        }
        v
    }

    #[test]
    fn default_is_openh264_and_first_packet_is_idr() {
        let mut e = AdaptiveEncoder::new(EncoderSettings::default(), 320, 240).unwrap();
        assert_eq!(e.active(), EncoderKind::OpenH264);
        let p = e.encode(&frame(&pattern(320, 240, 0), 320, 240, 0)).unwrap().expect("emits immediately");
        assert!(p.keyframe);
        assert!(e.take_events().is_empty());
    }

    #[test]
    fn missing_hw_encoder_falls_back_at_init() {
        let s = EncoderSettings { encoder: EncoderKind::MediaFoundation, ..Default::default() };
        let mut e = AdaptiveEncoder::new(s, 320, 240).unwrap();
        assert_eq!(e.active(), FALLBACK);
        assert!(e.is_fallback());
        let ev = e.take_events();
        assert_eq!(ev.len(), 1);
        assert_eq!(ev[0].from, EncoderKind::MediaFoundation);
        assert_eq!(ev[0].to, FALLBACK);
    }

    /// With an NVIDIA GPU: NVENC encodes, first packet IDR with SPS/PPS, bitrate changes in place.
    /// Without (or not Windows): OpenH264 with one fallback event. Either way the stream starts.
    #[test]
    fn nvenc_or_fallback() {
        let s = EncoderSettings { encoder: EncoderKind::Nvenc, retry_requested_secs: 0, ..Default::default() };
        let mut e = AdaptiveEncoder::new(s, 640, 360).unwrap();
        let ev = e.take_events();
        eprintln!("nvenc test: active {} ({}), events {ev:?}", e.active(), e.describe());
        let p = e.encode(&frame(&pattern(640, 360, 0), 640, 360, 0)).unwrap().expect("first frame");
        assert!(p.keyframe);
        if e.active() == EncoderKind::Nvenc {
            assert!(ev.is_empty());
            let has_sps = p.data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 0x1f == 7);
            assert!(has_sps, "SPS (NAL type 7) in the IDR");
            for t in 1..10 {
                let p = e.encode(&frame(&pattern(640, 360, t), 640, 360, t as i64)).unwrap().expect("one packet per frame");
                assert_eq!(p.encoder, EncoderKind::Nvenc);
            }
            e.set_bitrate(5000).unwrap();
            assert_eq!(e.active(), EncoderKind::Nvenc, "bitrate changed in place");
            assert!(e.describe().ends_with("5000 kbps"));
        } else {
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].from, EncoderKind::Nvenc);
        }
    }

    #[test]
    fn fallback_encoder_failure_restarts_it_with_an_idr() {
        let mut e = AdaptiveEncoder::new(EncoderSettings { retry_requested_secs: 0, ..Default::default() }, 320, 240).unwrap();
        for t in 0..3 {
            e.encode(&frame(&pattern(320, 240, t), 320, 240, t as i64)).unwrap();
        }
        e.inject_runtime_failures(1);
        let p = e.encode(&frame(&pattern(320, 240, 9), 320, 240, 9)).unwrap().unwrap();
        assert_eq!(p.encoder, FALLBACK);
        assert!(p.keyframe, "first packet of the restarted encoder must be an IDR");
        let ev = e.take_events();
        assert!(matches!(ev[0].phase, FallbackPhase::Runtime));
        assert_eq!((ev[0].from, ev[0].to), (FALLBACK, FALLBACK));
    }

    /// A frame the driver converted to I420 encodes to the same stream as the BGRA original
    /// (the GPU shader uses the CPU conversion's integer formula).
    #[test]
    fn i420_frames_encode_like_their_bgra_source() {
        let (w, h) = (320usize, 240usize);
        let mut a = AdaptiveEncoder::new(EncoderSettings::default(), w as u32, h as u32).unwrap();
        let mut b = AdaptiveEncoder::new(EncoderSettings::default(), w as u32, h as u32).unwrap();
        for t in 0..5 {
            let bgra = pattern(w, h, t);
            let mut yuv = convert::I420::new(w, h);
            convert::bgra_to_i420(&bgra, w * 4, &mut yuv, 0);
            let planar = [yuv.y, yuv.u, yuv.v].concat();
            let i420 = Frame { width: w as u32, height: h as u32, stride: w, format: PixelFormat::I420, data: &planar, pts_us: t as i64 };
            let pa = a.encode(&frame(&bgra, w as u32, h as u32, t as i64)).unwrap().map(|p| p.data);
            let pb = b.encode(&i420).unwrap().map(|p| p.data);
            assert_eq!(pa, pb, "frame {t}");
        }
        let short = Frame { width: w as u32, height: h as u32, stride: w, format: PixelFormat::I420, data: &[0; 10], pts_us: 9 };
        assert!(matches!(b.encode(&short), Err(EncodeError::InvalidInput(_))));
    }

    #[test]
    fn odd_sizes_are_rejected_not_fallen_back() {
        assert!(matches!(AdaptiveEncoder::new(EncoderSettings::default(), 321, 240), Err(EncodeError::InvalidInput(_))));
    }
}
