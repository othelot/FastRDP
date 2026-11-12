//! Encoder backends. OpenH264 takes BGRA, converts to I420 on the CPU and encodes; NVENC
//! (`crate::nvenc`) takes BGRA and converts on the GPU. Media Foundation is NOT implemented yet;
//! it reports `Unavailable`, which makes `AdaptiveEncoder` fall back to OpenH264 - by design.

use crate::convert::{I420, bgra_to_i420};
use crate::settings::{EncoderKind, EncoderSettings};
use crate::{EncodeError, Frame, Packet};

pub trait Backend: Send {
    fn kind(&self) -> EncoderKind;
    fn describe(&self) -> String;
    fn encode(&mut self, frame: &Frame<'_>, force_idr: bool) -> Result<Option<Packet>, EncodeError>;
    /// Changes the target bitrate in place; false = not supported (the caller re-creates the encoder).
    fn set_bitrate(&mut self, _kbps: u32) -> bool {
        false
    }
}

pub fn create(kind: EncoderKind, s: &EncoderSettings, width: u32, height: u32) -> Result<Box<dyn Backend>, EncodeError> {
    if width == 0 || height == 0 || !width.is_multiple_of(2) || !height.is_multiple_of(2) {
        return Err(EncodeError::InvalidInput(format!("unsupported size {width}x{height} (must be even)")));
    }
    match kind {
        EncoderKind::OpenH264 => Ok(Box::new(OpenH264Enc::new(s, width, height)?)),
        #[cfg(windows)]
        EncoderKind::Nvenc => Ok(Box::new(crate::nvenc::Nvenc::new(s, width, height)?)),
        #[cfg(not(windows))]
        EncoderKind::Nvenc => Err(EncodeError::Unavailable("NVENC: Windows only".into())),
        EncoderKind::MediaFoundation => Err(EncodeError::Unavailable("Media Foundation hardware H.264 backend not implemented yet".into())),
    }
}

// ------------------------------------------------------------------------------------------------ OpenH264
/// Slice size limit: frames split into slices that OpenH264 encodes in parallel.
const SLICE_BYTES: u32 = 20_000;

pub struct OpenH264Enc {
    enc: openh264::encoder::Encoder,
    yuv: I420,
    desc: String,
}

impl OpenH264Enc {
    pub fn new(s: &EncoderSettings, width: u32, height: u32) -> Result<Self, EncodeError> {
        use openh264::encoder::{BitRate, Complexity, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType};
        let o = &s.openh264;
        let usage = if o.usage.eq_ignore_ascii_case("camera") { UsageType::CameraVideoRealTime } else { UsageType::ScreenContentRealTime };
        let complexity = match o.complexity.to_ascii_lowercase().as_str() {
            "high" => Complexity::High,
            "medium" => Complexity::Medium,
            _ => Complexity::Low,
        };
        // Measured with encoder-bench (desktop-like content, 1080p-4K): the screen-content mode is
        // single-threaded and ~3x slower; size-limited slices let the camera mode use one thread per
        // slice (~2x faster, same PSNR); adaptive quantization / background detection cost ~6 dB.
        let mut cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(s.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(s.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(usage)
            .skip_frames(false)
            .complexity(complexity)
            .adaptive_quantization(false)
            .background_detection(false)
            .scene_change_detect(false)
            .num_threads(o.threads)
            .intra_frame_period(IntraFramePeriod::from_num_frames(s.keyint_secs * s.fps));
        if o.threads != 1 {
            cfg = cfg.max_slice_len(SLICE_BYTES);
        }
        let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), cfg).map_err(|e| EncodeError::InitFailed(format!("openh264: {e}")))?;
        Ok(OpenH264Enc {
            enc,
            yuv: I420::new(width as usize, height as usize),
            desc: format!(
                "OpenH264 (source build, BSD) usage={} complexity={} threads={} {}x{}@{} {} kbps",
                o.usage, o.complexity, o.threads, width, height, s.fps, s.bitrate_kbps
            ),
        })
    }
}

impl Backend for OpenH264Enc {
    fn kind(&self) -> EncoderKind {
        EncoderKind::OpenH264
    }
    fn describe(&self) -> String {
        self.desc.clone()
    }
    fn encode(&mut self, f: &Frame<'_>, force_idr: bool) -> Result<Option<Packet>, EncodeError> {
        use openh264::encoder::FrameType;
        use openh264::formats::YUVSlices;
        if f.width as usize != self.yuv.width || f.height as usize != self.yuv.height {
            return Err(EncodeError::InvalidInput("frame size changed".into()));
        }
        bgra_to_i420(f.bgra, f.stride, &mut self.yuv, 0);
        if force_idr {
            self.enc.force_intra_frame();
        }
        let y = &self.yuv;
        let src = YUVSlices::new((&y.y, &y.u, &y.v), (y.width, y.height), (y.y_stride(), y.c_stride(), y.c_stride()));
        let bs = self.enc.encode(&src).map_err(|e| EncodeError::Runtime(format!("openh264 encode: {e}")))?;
        let ft = bs.frame_type();
        if matches!(ft, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let data = bs.to_vec();
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Packet { data, keyframe: matches!(ft, FrameType::IDR | FrameType::I), pts_us: f.pts_us, encoder: EncoderKind::OpenH264 }))
    }
}
