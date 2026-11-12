//! Windows implementation of the NVENC backend (see the module docs in `mod.rs`).

use std::ffi::{CStr, c_char, c_void};

use windows_sys::Win32::Foundation::{FreeLibrary, HMODULE};
use windows_sys::Win32::System::LibraryLoader::{GetProcAddress, LOAD_LIBRARY_SEARCH_SYSTEM32, LoadLibraryExW};

use super::sys;
use crate::backends::Backend;
use crate::settings::{EncoderKind, EncoderSettings};
use crate::{EncodeError, Frame, Packet, PixelFormat};

/// Loaded system DLL, freed on drop.
struct Lib(HMODULE);

impl Lib {
    fn load(name: &str) -> Option<Lib> {
        let w: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
        // SAFETY: system DLL by name, searched in System32 only.
        let h = unsafe { LoadLibraryExW(w.as_ptr(), std::ptr::null_mut(), LOAD_LIBRARY_SEARCH_SYSTEM32) };
        (!h.is_null()).then_some(Lib(h))
    }

    /// # Safety
    /// `F` must be the function pointer type of the export `name`.
    unsafe fn get<F: Copy>(&self, name: &CStr) -> Option<F> {
        assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<usize>());
        // SAFETY: valid module handle and NUL-terminated name.
        let p = unsafe { GetProcAddress(self.0, name.as_ptr().cast()) }?;
        // SAFETY: the caller names the matching function type; both are plain function pointers.
        Some(unsafe { std::mem::transmute_copy(&p) })
    }
}

impl Drop for Lib {
    fn drop(&mut self) {
        // SAFETY: handle from LoadLibraryExW, freed once.
        unsafe { FreeLibrary(self.0) };
    }
}

type CuResult = i32;
type CuContext = *mut c_void;

/// The few CUDA driver API calls needed for an NVENC device.
struct Cuda {
    ctx: CuContext,
    push: unsafe extern "system" fn(CuContext) -> CuResult,
    pop: unsafe extern "system" fn(*mut CuContext) -> CuResult,
    destroy: unsafe extern "system" fn(CuContext) -> CuResult,
    name: String,
    _lib: Lib,
}

impl Cuda {
    fn open() -> Result<Cuda, EncodeError> {
        let lib = Lib::load("nvcuda.dll").ok_or_else(|| EncodeError::Unavailable("NVENC: nvcuda.dll not present (no NVIDIA driver)".into()))?;
        let missing = |f: &str| EncodeError::Unavailable(format!("NVENC: nvcuda.dll has no {f}"));
        // SAFETY: CUDA driver API signatures (CUDAAPI = __stdcall = system ABI on x64).
        unsafe {
            let init: unsafe extern "system" fn(u32) -> CuResult = lib.get(c"cuInit").ok_or_else(|| missing("cuInit"))?;
            let device_get: unsafe extern "system" fn(*mut i32, i32) -> CuResult = lib.get(c"cuDeviceGet").ok_or_else(|| missing("cuDeviceGet"))?;
            let device_name: unsafe extern "system" fn(*mut c_char, i32, i32) -> CuResult =
                lib.get(c"cuDeviceGetName").ok_or_else(|| missing("cuDeviceGetName"))?;
            let create: unsafe extern "system" fn(*mut CuContext, u32, i32) -> CuResult =
                lib.get(c"cuCtxCreate_v2").ok_or_else(|| missing("cuCtxCreate_v2"))?;
            let destroy = lib.get(c"cuCtxDestroy_v2").ok_or_else(|| missing("cuCtxDestroy_v2"))?;
            let push = lib.get(c"cuCtxPushCurrent_v2").ok_or_else(|| missing("cuCtxPushCurrent_v2"))?;
            let pop: unsafe extern "system" fn(*mut CuContext) -> CuResult = lib.get(c"cuCtxPopCurrent_v2").ok_or_else(|| missing("cuCtxPopCurrent_v2"))?;
            let rc = init(0);
            if rc != 0 {
                return Err(EncodeError::Unavailable(format!("NVENC: cuInit failed ({rc}), no usable NVIDIA GPU")));
            }
            let mut dev = 0;
            let rc = device_get(&mut dev, 0);
            if rc != 0 {
                return Err(EncodeError::Unavailable(format!("NVENC: no CUDA device ({rc})")));
            }
            let mut buf = [0 as c_char; 128];
            let name = if device_name(buf.as_mut_ptr(), buf.len() as i32, dev) == 0 {
                CStr::from_ptr(buf.as_ptr()).to_string_lossy().into_owned()
            } else {
                "NVIDIA GPU".into()
            };
            let mut ctx = std::ptr::null_mut();
            let rc = create(&mut ctx, 0, dev);
            if rc != 0 || ctx.is_null() {
                return Err(EncodeError::InitFailed(format!("NVENC: cuCtxCreate failed ({rc})")));
            }
            // created contexts are current: detach, every call pushes it (the encoder may move threads)
            let mut cur = std::ptr::null_mut();
            pop(&mut cur);
            Ok(Cuda { ctx, push, pop, destroy, name, _lib: lib })
        }
    }

    /// Makes the context current on this thread until the guard is dropped.
    fn bind(&self) -> CtxGuard {
        // SAFETY: valid context from cuCtxCreate.
        unsafe { (self.push)(self.ctx) };
        CtxGuard(self.pop)
    }
}

/// Pops the context pushed by `Cuda::bind` (does not borrow the owner, which stays usable).
struct CtxGuard(unsafe extern "system" fn(*mut CuContext) -> CuResult);

impl Drop for CtxGuard {
    fn drop(&mut self) {
        let mut cur = std::ptr::null_mut();
        // SAFETY: pops the context pushed by `bind`.
        unsafe { (self.0)(&mut cur) };
    }
}

impl Drop for Cuda {
    fn drop(&mut self) {
        // SAFETY: context from cuCtxCreate, destroyed once (after the encoder, see Nvenc::drop).
        unsafe { (self.destroy)(self.ctx) };
    }
}

/// The NVENC functions this backend calls (all checked present at open).
struct Api {
    f: Box<sys::FunctionList>,
    _lib: Lib,
}

macro_rules! call {
    ($api:expr, $f:ident ( $($arg:expr),* )) => {
        // SAFETY: the pointer was checked at open; arguments follow nvEncodeAPI.h.
        unsafe { ($api.f.$f.expect(stringify!($f)))($($arg),*) }
    };
}

fn status_name(s: sys::Status) -> &'static str {
    const NAMES: [&str; 26] = [
        "SUCCESS",
        "NO_ENCODE_DEVICE",
        "UNSUPPORTED_DEVICE",
        "INVALID_ENCODERDEVICE",
        "INVALID_DEVICE",
        "DEVICE_NOT_EXIST",
        "INVALID_PTR",
        "INVALID_EVENT",
        "INVALID_PARAM",
        "INVALID_CALL",
        "OUT_OF_MEMORY",
        "ENCODER_NOT_INITIALIZED",
        "UNSUPPORTED_PARAM",
        "LOCK_BUSY",
        "NOT_ENOUGH_BUFFER",
        "INVALID_VERSION",
        "MAP_FAILED",
        "NEED_MORE_INPUT",
        "ENCODER_BUSY",
        "EVENT_NOT_REGISTERD",
        "GENERIC",
        "INCOMPATIBLE_CLIENT_KEY",
        "UNIMPLEMENTED",
        "RESOURCE_REGISTER_FAILED",
        "RESOURCE_NOT_REGISTERED",
        "RESOURCE_NOT_MAPPED",
    ];
    usize::try_from(s).ok().and_then(|i| NAMES.get(i)).copied().unwrap_or("UNKNOWN")
}

impl Api {
    fn open() -> Result<Api, EncodeError> {
        let lib = Lib::load("nvEncodeAPI64.dll").ok_or_else(|| EncodeError::Unavailable("NVENC: nvEncodeAPI64.dll not present (no NVIDIA driver)".into()))?;
        // SAFETY: exported NVENCODE API entry points (NVENCAPI = __stdcall = system ABI on x64).
        unsafe {
            let max: unsafe extern "system" fn(*mut u32) -> sys::Status = lib
                .get(c"NvEncodeAPIGetMaxSupportedVersion")
                .ok_or_else(|| EncodeError::Unavailable("NVENC: NvEncodeAPIGetMaxSupportedVersion missing".into()))?;
            let create: unsafe extern "system" fn(*mut sys::FunctionList) -> sys::Status =
                lib.get(c"NvEncodeAPICreateInstance").ok_or_else(|| EncodeError::Unavailable("NVENC: NvEncodeAPICreateInstance missing".into()))?;
            let mut v = 0u32;
            let rc = max(&mut v);
            let need = (sys::API_MAJOR << 4) | sys::API_MINOR;
            if rc != sys::SUCCESS || v < need {
                return Err(EncodeError::Unavailable(format!(
                    "NVENC: driver supports API {}.{}, {}.{} needed (update the NVIDIA driver to 522 or newer)",
                    v >> 4,
                    v & 0xf,
                    sys::API_MAJOR,
                    sys::API_MINOR
                )));
            }
            let mut f = sys::zeroed::<sys::FunctionList>();
            f.version = sys::FUNCTION_LIST_VER;
            let rc = create(&mut *f);
            if rc != sys::SUCCESS {
                return Err(EncodeError::Unavailable(format!("NVENC: NvEncodeAPICreateInstance {}", status_name(rc))));
            }
            let present = f.open_encode_session_ex.is_some()
                && f.get_encode_preset_config_ex.is_some()
                && f.initialize_encoder.is_some()
                && f.create_input_buffer.is_some()
                && f.destroy_input_buffer.is_some()
                && f.create_bitstream_buffer.is_some()
                && f.destroy_bitstream_buffer.is_some()
                && f.encode_picture.is_some()
                && f.lock_bitstream.is_some()
                && f.unlock_bitstream.is_some()
                && f.lock_input_buffer.is_some()
                && f.unlock_input_buffer.is_some()
                && f.destroy_encoder.is_some()
                && f.reconfigure_encoder.is_some()
                && f.get_last_error_string.is_some();
            if !present {
                return Err(EncodeError::Unavailable("NVENC: function table incomplete".into()));
            }
            Ok(Api { f, _lib: lib })
        }
    }
}

pub struct Nvenc {
    // Drop::drop destroys the session first; then the fields drop in this order, the CUDA
    // context (and nvcuda.dll) last.
    enc: sys::Encoder,
    /// input buffers by frame format (created on first use): [ARGB, IYUV]
    inputs: [sys::InputPtr; 2],
    output: sys::OutputPtr,
    /// kept for nvEncReconfigureEncoder (`init.encode_config` points into `config`)
    init: Box<sys::InitializeParams>,
    config: Box<sys::Config>,
    width: u32,
    height: u32,
    fps: u32,
    frame_idx: u32,
    /// description without the bitrate
    desc: String,
    kbps: u32,
    api: Api,
    cuda: Cuda,
}

// SAFETY: the NVENC session and CUDA context are used from one thread at a time (&mut self), and
// every call binds the context to the calling thread.
unsafe impl Send for Nvenc {}

impl Nvenc {
    fn error(&self, what: &str, rc: sys::Status) -> String {
        let detail = if self.enc.is_null() {
            String::new()
        } else {
            // SAFETY: the API returns a NUL-terminated string owned by the session (or null).
            let p = call!(self.api, get_last_error_string(self.enc));
            if p.is_null() { String::new() } else { unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned() }
        };
        format!("NVENC {what}: {}{}{detail}", status_name(rc), if detail.is_empty() { "" } else { ": " })
    }

    pub fn new(s: &EncoderSettings, width: u32, height: u32) -> Result<Self, EncodeError> {
        let preset_n = s.nvenc.preset_number().map_err(EncodeError::InvalidInput)?;
        let api = Api::open()?;
        let cuda = Cuda::open()?;
        let mut me = Nvenc {
            enc: std::ptr::null_mut(),
            inputs: [std::ptr::null_mut(); 2],
            output: std::ptr::null_mut(),
            init: sys::zeroed(),
            config: sys::zeroed(),
            width,
            height,
            fps: s.fps,
            frame_idx: 0,
            desc: String::new(),
            kbps: s.bitrate_kbps,
            api,
            cuda,
        };
        let _ctx = me.cuda.bind();
        // session
        let mut open = sys::zeroed::<sys::OpenEncodeSessionExParams>();
        open.version = sys::OPEN_ENCODE_SESSION_EX_PARAMS_VER;
        open.device_type = sys::DEVICE_TYPE_CUDA;
        open.device = me.cuda.ctx;
        open.api_version = sys::API_VERSION;
        let mut enc = std::ptr::null_mut();
        let rc = call!(me.api, open_encode_session_ex(&mut *open, &mut enc));
        if rc != sys::SUCCESS || enc.is_null() {
            // e.g. too many sessions on consumer GPUs, or no encoder on this GPU
            return Err(EncodeError::Unavailable(me.error("open session", rc)));
        }
        me.enc = enc;
        // preset configuration, adjusted for low-latency streaming
        let preset_guid = sys::PRESET_GUIDS[preset_n - 1];
        let mut preset = sys::zeroed::<sys::PresetConfig>();
        preset.version = sys::PRESET_CONFIG_VER;
        preset.preset_cfg.version = sys::CONFIG_VER;
        let rc = call!(me.api, get_encode_preset_config_ex(me.enc, sys::CODEC_H264_GUID, preset_guid, sys::TUNING_INFO_ULTRA_LOW_LATENCY, &mut *preset));
        if rc != sys::SUCCESS {
            return Err(EncodeError::InitFailed(me.error("preset config", rc)));
        }
        *me.config = preset.preset_cfg;
        let c = &mut *me.config;
        c.version = sys::CONFIG_VER;
        c.profile_guid = sys::H264_PROFILE_HIGH_GUID;
        let gop = if s.keyint_secs > 0 { s.keyint_secs * s.fps } else { sys::INFINITE_GOPLENGTH };
        c.gop_length = gop;
        c.frame_interval_p = 1; // no B-frames
        c.rc_params.version = sys::RC_PARAMS_VER;
        set_rate(&mut c.rc_params, s.bitrate_kbps, s.fps);
        let h = c.encode_codec_config.h264();
        h.idr_period = gop;
        h.flags |= sys::H264_REPEAT_SPSPPS;
        h.entropy_coding_mode = sys::H264_ENTROPY_CABAC;
        // Level 5.2 at most: automatic selection writes 6.x into the SPS for e.g. 3456x2160@120
        // (macroblock rate above 5.2's), and OpenH264 - the client's decoder - rejects every 6.x
        // stream (black picture). The decoder only needs the level for its buffer sizes.
        h.level = sys::H264_LEVEL_52;
        // initialization
        let init = &mut *me.init;
        init.version = sys::INITIALIZE_PARAMS_VER;
        init.encode_guid = sys::CODEC_H264_GUID;
        init.preset_guid = preset_guid;
        init.encode_width = width;
        init.encode_height = height;
        init.dar_width = width;
        init.dar_height = height;
        init.frame_rate_num = s.fps;
        init.frame_rate_den = 1;
        init.enable_encode_async = 0;
        init.enable_ptd = 1;
        init.encode_config = &mut *me.config;
        init.max_encode_width = width;
        init.max_encode_height = height;
        init.tuning_info = sys::TUNING_INFO_ULTRA_LOW_LATENCY;
        init.buffer_format = sys::BUFFER_FORMAT_ARGB;
        let rc = call!(me.api, initialize_encoder(me.enc, &mut *me.init));
        if rc != sys::SUCCESS {
            return Err(EncodeError::InitFailed(me.error("initialize", rc)));
        }
        // output buffer (input buffers: per frame format, see `input`)
        let mut cbs = sys::zeroed::<sys::CreateBitstreamBuffer>();
        cbs.version = sys::CREATE_BITSTREAM_BUFFER_VER;
        let rc = call!(me.api, create_bitstream_buffer(me.enc, &mut *cbs));
        if rc != sys::SUCCESS {
            return Err(EncodeError::InitFailed(me.error("bitstream buffer", rc)));
        }
        me.output = cbs.bitstream_buffer;
        me.desc = format!("NVENC {} p{} ultra-low-latency CBR {}x{}@{}", me.cuda.name, preset_n, width, height, s.fps);
        Ok(me)
    }

    /// The input buffer for a frame format (NVENC buffer format, buffer), created on first use.
    fn input(&mut self, format: PixelFormat) -> Result<(u32, sys::InputPtr), EncodeError> {
        let (i, fmt) = match format {
            PixelFormat::Bgra => (0, sys::BUFFER_FORMAT_ARGB),
            PixelFormat::I420 => (1, sys::BUFFER_FORMAT_IYUV),
        };
        if self.inputs[i].is_null() {
            let mut cin = sys::zeroed::<sys::CreateInputBuffer>();
            cin.version = sys::CREATE_INPUT_BUFFER_VER;
            cin.width = self.width;
            cin.height = self.height;
            cin.buffer_fmt = fmt;
            let rc = call!(self.api, create_input_buffer(self.enc, &mut *cin));
            if rc != sys::SUCCESS || cin.input_buffer.is_null() {
                return Err(EncodeError::Runtime(self.error("input buffer", rc)));
            }
            self.inputs[i] = cin.input_buffer;
        }
        Ok((fmt, self.inputs[i]))
    }

    /// Copies the frame into its input buffer; returns (buffer format, buffer, luma pitch in bytes).
    fn upload(&mut self, f: &Frame<'_>) -> Result<(u32, sys::InputPtr, u32), EncodeError> {
        let (fmt, input) = self.input(f.format)?;
        let mut lock = sys::zeroed::<sys::LockInputBuffer>();
        lock.version = sys::LOCK_INPUT_BUFFER_VER;
        lock.input_buffer = input;
        let rc = call!(self.api, lock_input_buffer(self.enc, &mut *lock));
        if rc != sys::SUCCESS || lock.buffer_data_ptr.is_null() {
            return Err(EncodeError::Runtime(self.error("lock input", rc)));
        }
        let (w, h, pitch) = (self.width as usize, self.height as usize, lock.pitch as usize);
        let dst = lock.buffer_data_ptr.cast::<u8>();
        // (source rows, row length, destination offset, destination pitch) per plane
        let copy = |src: &[u8], src_pitch: usize, rows: usize, row: usize, at: usize, dst_pitch: usize| {
            for y in 0..rows {
                let s = &src[y * src_pitch..y * src_pitch + row];
                // SAFETY: the locked buffer holds the planes of a `height`-row surface with this
                // pitch (IYUV: chroma planes after the luma plane, half pitch, half height).
                unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), dst.add(at + y * dst_pitch), row) };
            }
        };
        let ok = match f.format {
            PixelFormat::Bgra => {
                let enough = f.data.len() >= f.stride * (h - 1) + w * 4;
                if enough {
                    copy(f.data, f.stride, h, w * 4, 0, pitch);
                }
                enough
            }
            PixelFormat::I420 => match f.i420_planes() {
                Some((y, u, v)) => {
                    let (cw, ch, cp) = (w / 2, h / 2, pitch / 2);
                    copy(y, w, h, w, 0, pitch);
                    copy(u, cw, ch, cw, pitch * h, cp);
                    copy(v, cw, ch, cw, pitch * h + cp * ch, cp);
                    true
                }
                None => false,
            },
        };
        let rc = call!(self.api, unlock_input_buffer(self.enc, input));
        if !ok {
            return Err(EncodeError::InvalidInput("frame buffer too small".into()));
        }
        if rc != sys::SUCCESS {
            return Err(EncodeError::Runtime(self.error("unlock input", rc)));
        }
        Ok((fmt, input, lock.pitch))
    }
}

/// CBR at `kbps` with a one-frame VBV (lowest latency; frames do not queue in the network).
fn set_rate(rc: &mut sys::RcParams, kbps: u32, fps: u32) {
    let bps = kbps.saturating_mul(1000);
    rc.rate_control_mode = sys::RC_CBR;
    rc.average_bit_rate = bps;
    rc.max_bit_rate = bps;
    rc.vbv_buffer_size = bps / fps.max(1);
    rc.vbv_initial_delay = rc.vbv_buffer_size;
}

impl Drop for Nvenc {
    fn drop(&mut self) {
        let _ctx = self.cuda.bind();
        if !self.enc.is_null() {
            for input in self.inputs {
                if !input.is_null() {
                    call!(self.api, destroy_input_buffer(self.enc, input));
                }
            }
            if !self.output.is_null() {
                call!(self.api, destroy_bitstream_buffer(self.enc, self.output));
            }
            call!(self.api, destroy_encoder(self.enc));
        }
        // then `cuda` (context) and the libraries drop
    }
}

impl Backend for Nvenc {
    fn kind(&self) -> EncoderKind {
        EncoderKind::Nvenc
    }

    fn describe(&self) -> String {
        format!("{} {} kbps", self.desc, self.kbps)
    }

    fn set_bitrate(&mut self, kbps: u32) -> bool {
        let _ctx = self.cuda.bind();
        set_rate(&mut self.config.rc_params, kbps, self.fps);
        let mut re = sys::zeroed::<sys::ReconfigureParams>();
        re.version = sys::RECONFIGURE_PARAMS_VER;
        re.re_init_encode_params = *self.init;
        re.re_init_encode_params.encode_config = &mut *self.config;
        let ok = call!(self.api, reconfigure_encoder(self.enc, &mut *re)) == sys::SUCCESS;
        if ok {
            self.kbps = kbps;
        }
        ok
    }

    fn encode(&mut self, f: &Frame<'_>, force_idr: bool) -> Result<Option<Packet>, EncodeError> {
        if f.width != self.width || f.height != self.height {
            return Err(EncodeError::InvalidInput("frame size changed".into()));
        }
        let _ctx = self.cuda.bind();
        let (fmt, input, pitch) = self.upload(f)?;
        let mut pic = sys::zeroed::<sys::PicParams>();
        pic.version = sys::PIC_PARAMS_VER;
        pic.input_width = self.width;
        pic.input_height = self.height;
        pic.input_pitch = pitch; // bytes, as locked (FFmpeg's nvenc passes the lock pitch too)
        pic.encode_pic_flags = if force_idr { sys::PIC_FLAG_FORCEIDR | sys::PIC_FLAG_OUTPUT_SPSPPS } else { 0 };
        pic.frame_idx = self.frame_idx;
        pic.input_time_stamp = f.pts_us as u64;
        pic.input_buffer = input;
        pic.output_bitstream = self.output;
        pic.buffer_fmt = fmt;
        pic.picture_struct = sys::PIC_STRUCT_FRAME;
        self.frame_idx = self.frame_idx.wrapping_add(1);
        let rc = call!(self.api, encode_picture(self.enc, &mut *pic));
        if rc == sys::ERR_NEED_MORE_INPUT {
            return Ok(None);
        }
        if rc != sys::SUCCESS {
            return Err(EncodeError::Runtime(self.error("encode", rc)));
        }
        let mut lock = sys::zeroed::<sys::LockBitstream>();
        lock.version = sys::LOCK_BITSTREAM_VER;
        lock.output_bitstream = self.output;
        let rc = call!(self.api, lock_bitstream(self.enc, &mut *lock));
        if rc != sys::SUCCESS {
            return Err(EncodeError::Runtime(self.error("lock bitstream", rc)));
        }
        let len = lock.bitstream_size_in_bytes as usize;
        let data = if len == 0 || lock.bitstream_buffer_ptr.is_null() {
            Vec::new()
        } else {
            // SAFETY: the locked bitstream holds `len` bytes until it is unlocked; copied now.
            unsafe { std::slice::from_raw_parts(lock.bitstream_buffer_ptr.cast::<u8>(), len) }.to_vec()
        };
        let keyframe = matches!(lock.picture_type, sys::PIC_TYPE_IDR | sys::PIC_TYPE_I);
        let rc = call!(self.api, unlock_bitstream(self.enc, self.output));
        if rc != sys::SUCCESS {
            return Err(EncodeError::Runtime(self.error("unlock bitstream", rc)));
        }
        if data.is_empty() {
            return Ok(None);
        }
        Ok(Some(Packet { data, keyframe, pts_us: f.pts_us, encoder: EncoderKind::Nvenc }))
    }
}
