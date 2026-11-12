//! Encoder backends. Software backends (x264, OpenH264) take BGRA, convert to I420 and encode.
//! Hardware backends (NVENC, Media Foundation) are probed but NOT implemented yet; they
//! report `Unavailable`, which makes `AdaptiveEncoder` fall back to x264 - by design.

use std::ffi::{CStr, CString, c_char, c_int};

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
        EncoderKind::X264 => Ok(Box::new(X264::new(s, width, height)?)),
        EncoderKind::OpenH264 => Ok(Box::new(OpenH264Enc::new(s, width, height)?)),
        EncoderKind::Nvenc => Err(probe_hw("nvEncodeAPI64.dll", "NVENC")),
        EncoderKind::MediaFoundation => Err(EncodeError::Unavailable("Media Foundation hardware H.264 backend not implemented yet".into())),
    }
}

fn probe_hw(dll: &str, name: &str) -> EncodeError {
    use windows_sys::Win32::Foundation::FreeLibrary;
    use windows_sys::Win32::System::LibraryLoader::{LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};
    let w: Vec<u16> = dll.encode_utf16().chain(Some(0)).collect();
    // SAFETY: probing a system DLL by name; freed immediately.
    let h = unsafe { LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
    if h.is_null() {
        EncodeError::Unavailable(format!("{name}: {dll} not present (no driver/GPU)"))
    } else {
        // SAFETY: handle from LoadLibraryExW.
        unsafe { FreeLibrary(h) };
        EncodeError::Unavailable(format!("{name}: {dll} present but backend not implemented yet"))
    }
}

// ------------------------------------------------------------------------------------------------ x264
#[repr(C)]
struct Fx264Params {
    width: c_int,
    height: c_int,
    fps: c_int,
    bitrate_kbps: c_int,
    vbv_ms: c_int,
    threads: c_int,
    keyint: c_int,
    intra_refresh: c_int,
    preset: *const c_char,
    tune: *const c_char,
    profile: *const c_char,
}

#[repr(C)]
struct Fx264 {
    _p: [u8; 0],
}

unsafe extern "C" {
    fn fx264_version() -> *const c_char;
    fn fx264_build() -> c_int;
    fn fx264_open(out: *mut *mut Fx264, p: *const Fx264Params, err: *mut c_char, errlen: c_int) -> c_int;
    #[allow(clippy::too_many_arguments)]
    fn fx264_encode(
        e: *mut Fx264,
        y: *const u8,
        ys: c_int,
        u: *const u8,
        us: c_int,
        v: *const u8,
        vs: c_int,
        pts: i64,
        force_idr: c_int,
        out: *mut *const u8,
        outlen: *mut c_int,
        keyframe: *mut c_int,
    ) -> c_int;
    fn fx264_close(e: *mut Fx264);
    fn fx264_set_bitrate(e: *mut Fx264, kbps: c_int, vbv_ms: c_int) -> c_int;
}

pub fn x264_version() -> String {
    // SAFETY: returns a static NUL-terminated string.
    let v = unsafe { CStr::from_ptr(fx264_version()) }.to_string_lossy().into_owned();
    // SAFETY: trivial.
    format!("{v} (build {})", unsafe { fx264_build() })
}

pub struct X264 {
    h: *mut Fx264,
    vbv_ms: u32,
    yuv: I420,
    desc: String,
}

// SAFETY: the x264 handle is used from one thread at a time (&mut self).
unsafe impl Send for X264 {}

impl X264 {
    pub fn new(s: &EncoderSettings, width: u32, height: u32) -> Result<Self, EncodeError> {
        let x = &s.x264;
        let preset = CString::new(x.preset.clone()).map_err(|_| EncodeError::InvalidInput("preset".into()))?;
        let tune = CString::new(x.tune.clone()).map_err(|_| EncodeError::InvalidInput("tune".into()))?;
        let profile = CString::new(x.profile.clone()).map_err(|_| EncodeError::InvalidInput("profile".into()))?;
        let p = Fx264Params {
            width: width as c_int,
            height: height as c_int,
            fps: s.fps as c_int,
            bitrate_kbps: s.bitrate_kbps as c_int,
            vbv_ms: x.vbv_ms as c_int,
            threads: x.threads as c_int,
            keyint: (s.keyint_secs * s.fps) as c_int,
            intra_refresh: x.intra_refresh as c_int,
            preset: preset.as_ptr(),
            tune: if x.tune.is_empty() { std::ptr::null() } else { tune.as_ptr() },
            profile: profile.as_ptr(),
        };
        let mut h = std::ptr::null_mut();
        let mut err = [0 as c_char; 256];
        // SAFETY: valid params/out pointers; err buffer length passed.
        let rc = unsafe { fx264_open(&mut h, &p, err.as_mut_ptr(), err.len() as c_int) };
        if rc != 0 || h.is_null() {
            // SAFETY: shim always NUL-terminates via snprintf.
            let msg = unsafe { CStr::from_ptr(err.as_ptr()) }.to_string_lossy().into_owned();
            return Err(EncodeError::InitFailed(format!("x264: {msg} (rc={rc})")));
        }
        Ok(X264 {
            h,
            vbv_ms: x.vbv_ms,
            yuv: I420::new(width as usize, height as usize),
            desc: format!(
                "x264 {} preset={} tune={} profile={} {}x{}@{} {} kbps",
                x264_version(),
                x.preset,
                x.tune,
                x.profile,
                width,
                height,
                s.fps,
                s.bitrate_kbps
            ),
        })
    }
}

impl Drop for X264 {
    fn drop(&mut self) {
        // SAFETY: handle from fx264_open, closed once.
        unsafe { fx264_close(self.h) };
    }
}

impl Backend for X264 {
    fn kind(&self) -> EncoderKind {
        EncoderKind::X264
    }
    fn set_bitrate(&mut self, kbps: u32) -> bool {
        // SAFETY: valid open encoder handle.
        unsafe { fx264_set_bitrate(self.h, kbps as c_int, self.vbv_ms as c_int) >= 0 }
    }
    fn describe(&self) -> String {
        self.desc.clone()
    }
    fn encode(&mut self, f: &Frame<'_>, force_idr: bool) -> Result<Option<Packet>, EncodeError> {
        if f.width as usize != self.yuv.width || f.height as usize != self.yuv.height {
            return Err(EncodeError::InvalidInput("frame size changed".into()));
        }
        bgra_to_i420(f.bgra, f.stride, &mut self.yuv, 0);
        let (mut out, mut len, mut key) = (std::ptr::null(), 0 as c_int, 0 as c_int);
        let y = &self.yuv;
        // SAFETY: planes are valid for the declared strides; outputs are plain out-params.
        let rc = unsafe {
            fx264_encode(
                self.h,
                y.y.as_ptr(),
                y.y_stride() as c_int,
                y.u.as_ptr(),
                y.c_stride() as c_int,
                y.v.as_ptr(),
                y.c_stride() as c_int,
                f.pts_us,
                force_idr as c_int,
                &mut out,
                &mut len,
                &mut key,
            )
        };
        if rc != 0 {
            return Err(EncodeError::Runtime(format!("x264_encoder_encode failed ({rc})")));
        }
        if len <= 0 || out.is_null() {
            return Ok(None);
        }
        // SAFETY: x264 guarantees `len` bytes at `out` until the next encode call; we copy now.
        let data = unsafe { std::slice::from_raw_parts(out, len as usize) }.to_vec();
        Ok(Some(Packet { data, keyframe: key != 0, pts_us: f.pts_us, encoder: EncoderKind::X264 }))
    }
}

// ------------------------------------------------------------------------------------------------ OpenH264
pub struct OpenH264Enc {
    enc: openh264::encoder::Encoder,
    yuv: I420,
    desc: String,
}

impl OpenH264Enc {
    pub fn new(s: &EncoderSettings, width: u32, height: u32) -> Result<Self, EncodeError> {
        use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType};
        let usage = if s.openh264.usage.eq_ignore_ascii_case("camera") { UsageType::CameraVideoRealTime } else { UsageType::ScreenContentRealTime };
        let cfg = EncoderConfig::new()
            .bitrate(BitRate::from_bps(s.bitrate_kbps.saturating_mul(1000)))
            .max_frame_rate(FrameRate::from_hz(s.fps as f32))
            .rate_control_mode(RateControlMode::Bitrate)
            .usage_type(usage)
            .skip_frames(false)
            .num_threads(s.openh264.threads)
            .intra_frame_period(IntraFramePeriod::from_num_frames(s.keyint_secs * s.fps));
        let enc = Encoder::with_api_config(openh264::OpenH264API::from_source(), cfg).map_err(|e| EncodeError::InitFailed(format!("openh264: {e}")))?;
        Ok(OpenH264Enc {
            enc,
            yuv: I420::new(width as usize, height as usize),
            desc: format!("OpenH264 (source build, BSD) usage={} {}x{}@{} {} kbps", s.openh264.usage, width, height, s.fps, s.bitrate_kbps),
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
