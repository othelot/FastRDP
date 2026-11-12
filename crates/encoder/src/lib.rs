//! FastRDP video encoding with a user-selected encoder and a **mandatory x264 fallback**.
//!
//! Policy (user requirement):
//! * the encoder is user-configurable (`encoder.toml`), **default = x264**;
//! * if the configured encoder is missing (no GPU/driver/DLL) or fails to initialise -> x264;
//! * if it fails at runtime (device removed, driver reset, session limit, encode error) -> switch to
//!   x264 immediately, re-encode the same frame as an IDR so the stream stays decodable;
//! * optionally re-try the configured encoder every `retry_requested_secs`.
//!
//! Every switch is recorded as a [`FallbackEvent`] for logs/telemetry/client notification.

pub mod backends;
pub mod convert;
pub mod settings;

use std::fmt;
use std::time::{Duration, Instant};

pub use settings::{EncoderKind, EncoderSettings};

/// One BGRA frame (e.g. a mapped IddCx swap-chain surface).
pub struct Frame<'a> {
    pub width: u32,
    pub height: u32,
    /// Bytes per row.
    pub stride: usize,
    pub bgra: &'a [u8],
    pub pts_us: i64,
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
    /// Creates the configured encoder; falls back to x264 when it is missing or fails.
    /// Errors only if x264 itself cannot be created.
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
            Err(e) if s.encoder != EncoderKind::X264 && !matches!(e, EncodeError::InvalidInput(_)) => {
                events.push(FallbackEvent { phase, from: s.encoder, to: EncoderKind::X264, reason: e.to_string() });
                backends::create(EncoderKind::X264, s, w, h)
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
            Err(e) if self.active.kind() != EncoderKind::X264 => {
                let from = self.active.kind();
                self.active = backends::create(EncoderKind::X264, &self.settings, self.width, self.height)?;
                self.events.push(FallbackEvent { phase: FallbackPhase::Runtime, from, to: EncoderKind::X264, reason: e.to_string() });
                self.fell_back_at = Some(Instant::now());
                // Same frame again, as IDR: the decoder must not depend on the dead encoder's references.
                let p = self.active.encode(f, true)?;
                self.force_idr = p.is_none();
                Ok(p)
            }
            Err(e) => {
                // x264 itself failed: recreate it once, then give up.
                self.active = backends::create(EncoderKind::X264, &self.settings, self.width, self.height)?;
                self.events.push(FallbackEvent {
                    phase: FallbackPhase::Runtime,
                    from: EncoderKind::X264,
                    to: EncoderKind::X264,
                    reason: format!("x264 restarted after: {e}"),
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
                from: EncoderKind::X264,
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
        Frame { width: w, height: h, stride: w as usize * 4, bgra: buf, pts_us: pts }
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
    fn default_is_x264_and_first_packet_is_idr() {
        let mut e = AdaptiveEncoder::new(EncoderSettings::default(), 320, 240).unwrap();
        assert_eq!(e.active(), EncoderKind::X264);
        let p = e.encode(&frame(&pattern(320, 240, 0), 320, 240, 0)).unwrap().expect("zerolatency emits immediately");
        assert!(p.keyframe);
        assert!(e.take_events().is_empty());
    }

    #[test]
    fn missing_hw_encoder_falls_back_to_x264_at_init() {
        for kind in [EncoderKind::Nvenc, EncoderKind::MediaFoundation] {
            let s = EncoderSettings { encoder: kind, ..Default::default() };
            let mut e = AdaptiveEncoder::new(s, 320, 240).unwrap();
            assert_eq!(e.active(), EncoderKind::X264);
            assert!(e.is_fallback());
            let ev = e.take_events();
            assert_eq!(ev.len(), 1);
            assert_eq!(ev[0].from, kind);
            assert_eq!(ev[0].to, EncoderKind::X264);
        }
    }

    #[test]
    fn openh264_runtime_failure_falls_back_to_x264_with_idr() {
        let s = EncoderSettings { encoder: EncoderKind::OpenH264, retry_requested_secs: 0, ..Default::default() };
        let mut e = AdaptiveEncoder::new(s, 320, 240).unwrap();
        assert_eq!(e.active(), EncoderKind::OpenH264);
        for t in 0..3 {
            e.encode(&frame(&pattern(320, 240, t), 320, 240, t as i64)).unwrap();
        }
        e.inject_runtime_failures(1);
        let p = e.encode(&frame(&pattern(320, 240, 9), 320, 240, 9)).unwrap().unwrap();
        assert_eq!(p.encoder, EncoderKind::X264);
        assert!(p.keyframe, "first x264 packet after fallback must be an IDR");
        let ev = e.take_events();
        assert!(matches!(ev[0].phase, FallbackPhase::Runtime));
        assert_eq!(ev[0].from, EncoderKind::OpenH264);
    }

    #[test]
    fn retry_returns_to_configured_encoder() {
        let s = EncoderSettings { encoder: EncoderKind::OpenH264, retry_requested_secs: 1, ..Default::default() };
        let mut e = AdaptiveEncoder::new(s, 160, 120).unwrap();
        e.inject_runtime_failures(1);
        e.encode(&frame(&pattern(160, 120, 0), 160, 120, 0)).unwrap();
        assert_eq!(e.active(), EncoderKind::X264);
        std::thread::sleep(Duration::from_millis(1100));
        let p = e.encode(&frame(&pattern(160, 120, 1), 160, 120, 1)).unwrap().unwrap();
        assert_eq!(e.active(), EncoderKind::OpenH264);
        assert!(p.keyframe);
    }

    #[test]
    fn odd_sizes_are_rejected_not_fallen_back() {
        assert!(matches!(AdaptiveEncoder::new(EncoderSettings::default(), 321, 240), Err(EncodeError::InvalidInput(_))));
    }
}
