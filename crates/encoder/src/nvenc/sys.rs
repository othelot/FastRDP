//! NVENCODE API 12.0 declarations (the subset FastRDP uses), transcribed from `nvEncodeAPI.h` of
//! FFmpeg's nv-codec-headers n12.0.16.1 (MIT, Copyright (c) 2010-2022 NVIDIA Corporation).
//! Structs keep the header's exact size: fields FastRDP does not touch are reserved arrays, and the
//! tests pin every size and used offset to values computed from the C header with clang.
//! API 12.0 needs NVIDIA driver 522 or newer; `nvEncodeAPI64.dll` ships with the driver.
#![allow(dead_code)]

use std::ffi::{c_char, c_void};

pub type Status = i32;
pub const SUCCESS: Status = 0;
pub const ERR_NEED_MORE_INPUT: Status = 17;

pub const API_MAJOR: u32 = 12;
pub const API_MINOR: u32 = 0;
pub const API_VERSION: u32 = API_MAJOR | (API_MINOR << 24);
const fn struct_version(v: u32) -> u32 {
    API_VERSION | (v << 16) | (0x7 << 28)
}
pub const FUNCTION_LIST_VER: u32 = struct_version(2);
pub const OPEN_ENCODE_SESSION_EX_PARAMS_VER: u32 = struct_version(1);
pub const PRESET_CONFIG_VER: u32 = struct_version(4) | (1 << 31);
pub const CONFIG_VER: u32 = struct_version(8) | (1 << 31);
pub const RC_PARAMS_VER: u32 = struct_version(1);
pub const INITIALIZE_PARAMS_VER: u32 = struct_version(5) | (1 << 31);
pub const CREATE_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const CREATE_BITSTREAM_BUFFER_VER: u32 = struct_version(1);
pub const LOCK_INPUT_BUFFER_VER: u32 = struct_version(1);
pub const LOCK_BITSTREAM_VER: u32 = struct_version(2);
pub const PIC_PARAMS_VER: u32 = struct_version(6) | (1 << 31);
pub const RECONFIGURE_PARAMS_VER: u32 = struct_version(1) | (1 << 31);

pub const INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
pub const DEVICE_TYPE_CUDA: u32 = 1;
pub const TUNING_INFO_ULTRA_LOW_LATENCY: u32 = 3;
pub const RC_CBR: u32 = 2;
/// 32-bit words with B in the lowest byte: BGRA in memory (the IddCx swap-chain format)
pub const BUFFER_FORMAT_ARGB: u32 = 0x0100_0000;
/// planar Y, U, V (I420); chroma planes follow the luma plane with half the pitch
pub const BUFFER_FORMAT_IYUV: u32 = 0x0000_0100;
pub const PIC_STRUCT_FRAME: u32 = 1;
pub const PIC_TYPE_I: u32 = 2;
pub const PIC_TYPE_IDR: u32 = 3;
pub const PIC_FLAG_FORCEIDR: u32 = 0x2;
pub const PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x4;
/// NV_ENC_CONFIG_H264 bit field: repeat SPS/PPS with every IDR (a client can join at any IDR)
pub const H264_REPEAT_SPSPPS: u32 = 1 << 12;
pub const H264_ENTROPY_CABAC: u32 = 1;
/// NV_ENC_LEVEL_H264_52: the highest level OpenH264 (the client's decoder) accepts
pub const H264_LEVEL_52: u32 = 52;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

pub const CODEC_H264_GUID: Guid = Guid { data1: 0x6bc82762, data2: 0x4e63, data3: 0x4ca4, data4: [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf] };
pub const H264_PROFILE_HIGH_GUID: Guid = Guid { data1: 0xe7cbc309, data2: 0x4f7a, data3: 0x4b89, data4: [0xaf, 0x2a, 0xd5, 0x37, 0xc9, 0x2b, 0xe3, 0x10] };
/// P1 (fastest) .. P7 (best quality)
pub const PRESET_GUIDS: [Guid; 7] = [
    Guid { data1: 0xfc0a8d3e, data2: 0x45f8, data3: 0x4cf8, data4: [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf] },
    Guid { data1: 0xf581cfb8, data2: 0x88d6, data3: 0x4381, data4: [0x93, 0xf0, 0xdf, 0x13, 0xf9, 0xc2, 0x7d, 0xab] },
    Guid { data1: 0x36850110, data2: 0x3a07, data3: 0x441f, data4: [0x94, 0xd5, 0x36, 0x70, 0x63, 0x1f, 0x91, 0xf6] },
    Guid { data1: 0x90a7b826, data2: 0xdf06, data3: 0x4862, data4: [0xb9, 0xd2, 0xcd, 0x6d, 0x73, 0xa0, 0x86, 0x81] },
    Guid { data1: 0x21c6e6b4, data2: 0x297a, data3: 0x4cba, data4: [0x99, 0x8f, 0xb6, 0xcb, 0xde, 0x72, 0xad, 0xe3] },
    Guid { data1: 0x8e75c279, data2: 0x6299, data3: 0x4ab6, data4: [0x83, 0x02, 0x0b, 0x21, 0x5a, 0x33, 0x5c, 0xf5] },
    Guid { data1: 0x84848c12, data2: 0x6f71, data3: 0x4c13, data4: [0x93, 0x1b, 0x53, 0xe2, 0x83, 0xf5, 0x79, 0x74] },
];

pub type Encoder = *mut c_void;
pub type InputPtr = *mut c_void;
pub type OutputPtr = *mut c_void;

pub type FnOpenEncodeSessionEx = unsafe extern "system" fn(*mut OpenEncodeSessionExParams, *mut Encoder) -> Status;
pub type FnGetEncodePresetConfigEx = unsafe extern "system" fn(Encoder, Guid, Guid, u32, *mut PresetConfig) -> Status;
pub type FnInitializeEncoder = unsafe extern "system" fn(Encoder, *mut InitializeParams) -> Status;
pub type FnCreateInputBuffer = unsafe extern "system" fn(Encoder, *mut CreateInputBuffer) -> Status;
pub type FnDestroyInputBuffer = unsafe extern "system" fn(Encoder, InputPtr) -> Status;
pub type FnCreateBitstreamBuffer = unsafe extern "system" fn(Encoder, *mut CreateBitstreamBuffer) -> Status;
pub type FnDestroyBitstreamBuffer = unsafe extern "system" fn(Encoder, OutputPtr) -> Status;
pub type FnEncodePicture = unsafe extern "system" fn(Encoder, *mut PicParams) -> Status;
pub type FnLockBitstream = unsafe extern "system" fn(Encoder, *mut LockBitstream) -> Status;
pub type FnUnlockBitstream = unsafe extern "system" fn(Encoder, OutputPtr) -> Status;
pub type FnLockInputBuffer = unsafe extern "system" fn(Encoder, *mut LockInputBuffer) -> Status;
pub type FnUnlockInputBuffer = unsafe extern "system" fn(Encoder, InputPtr) -> Status;
pub type FnDestroyEncoder = unsafe extern "system" fn(Encoder) -> Status;
pub type FnReconfigureEncoder = unsafe extern "system" fn(Encoder, *mut ReconfigureParams) -> Status;
pub type FnGetLastErrorString = unsafe extern "system" fn(Encoder) -> *const c_char;

/// NV_ENCODE_API_FUNCTION_LIST: 41 entries in header order, then reserved.
#[repr(C)]
pub struct FunctionList {
    pub version: u32,
    pub reserved: u32,
    pub open_encode_session: *const c_void,
    pub get_encode_guid_count: *const c_void,
    pub get_encode_profile_guid_count: *const c_void,
    pub get_encode_profile_guids: *const c_void,
    pub get_encode_guids: *const c_void,
    pub get_input_format_count: *const c_void,
    pub get_input_formats: *const c_void,
    pub get_encode_caps: *const c_void,
    pub get_encode_preset_count: *const c_void,
    pub get_encode_preset_guids: *const c_void,
    pub get_encode_preset_config: *const c_void,
    pub initialize_encoder: Option<FnInitializeEncoder>,
    pub create_input_buffer: Option<FnCreateInputBuffer>,
    pub destroy_input_buffer: Option<FnDestroyInputBuffer>,
    pub create_bitstream_buffer: Option<FnCreateBitstreamBuffer>,
    pub destroy_bitstream_buffer: Option<FnDestroyBitstreamBuffer>,
    pub encode_picture: Option<FnEncodePicture>,
    pub lock_bitstream: Option<FnLockBitstream>,
    pub unlock_bitstream: Option<FnUnlockBitstream>,
    pub lock_input_buffer: Option<FnLockInputBuffer>,
    pub unlock_input_buffer: Option<FnUnlockInputBuffer>,
    pub get_encode_stats: *const c_void,
    pub get_sequence_params: *const c_void,
    pub register_async_event: *const c_void,
    pub unregister_async_event: *const c_void,
    pub map_input_resource: *const c_void,
    pub unmap_input_resource: *const c_void,
    pub destroy_encoder: Option<FnDestroyEncoder>,
    pub invalidate_ref_frames: *const c_void,
    pub open_encode_session_ex: Option<FnOpenEncodeSessionEx>,
    pub register_resource: *const c_void,
    pub unregister_resource: *const c_void,
    pub reconfigure_encoder: Option<FnReconfigureEncoder>,
    pub reserved1: *const c_void,
    pub create_mv_buffer: *const c_void,
    pub destroy_mv_buffer: *const c_void,
    pub run_motion_estimation_only: *const c_void,
    pub get_last_error_string: Option<FnGetLastErrorString>,
    pub set_io_cuda_streams: *const c_void,
    pub get_encode_preset_config_ex: Option<FnGetEncodePresetConfigEx>,
    pub get_sequence_param_ex: *const c_void,
    pub reserved2: [*const c_void; 277],
}

#[repr(C)]
pub struct OpenEncodeSessionExParams {
    pub version: u32,
    pub device_type: u32,
    pub device: *mut c_void,
    pub reserved: *mut c_void,
    pub api_version: u32,
    pub reserved1: [u32; 253],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Qp {
    pub inter_p: u32,
    pub inter_b: u32,
    pub intra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct RcParams {
    pub version: u32,
    pub rate_control_mode: u32,
    pub const_qp: Qp,
    pub average_bit_rate: u32,
    pub max_bit_rate: u32,
    pub vbv_buffer_size: u32,
    pub vbv_initial_delay: u32,
    /// enableMinQP:1 enableMaxQP:1 enableInitialRCQP:1 enableAQ:1 ... aqStrength:4
    pub flags: u32,
    pub min_qp: Qp,
    pub max_qp: Qp,
    pub initial_rc_qp: Qp,
    pub temporal_layer_idx_mask: u32,
    pub temporal_layer_qp: [u8; 8],
    pub target_quality: u8,
    pub target_quality_lsb: u8,
    pub lookahead_depth: u16,
    pub low_delay_key_frame_scale: u8,
    pub y_dc_qp_index_offset: i8,
    pub u_dc_qp_index_offset: i8,
    pub v_dc_qp_index_offset: i8,
    pub qp_map_mode: u32,
    pub multi_pass: u32,
    pub alpha_layer_bitrate_ratio: u32,
    pub cb_qp_index_offset: i8,
    pub cr_qp_index_offset: i8,
    pub reserved2: u16,
    pub reserved: [u32; 4],
}

/// Leading fields of NV_ENC_CONFIG_H264 (the rest of the union stays as the preset set it).
#[repr(C)]
pub struct H264ConfigHead {
    /// bit fields; see `H264_REPEAT_SPSPPS`
    pub flags: u32,
    pub level: u32,
    pub idr_period: u32,
    pub separate_colour_plane_flag: u32,
    pub disable_deblocking_filter_idc: u32,
    pub num_temporal_layers: u32,
    pub sps_id: u32,
    pub pps_id: u32,
    pub adaptive_transform_mode: u32,
    pub fmo_mode: u32,
    pub bdirect_mode: u32,
    pub entropy_coding_mode: u32,
    pub stereo_mode: u32,
    pub intra_refresh_period: u32,
    pub intra_refresh_cnt: u32,
    pub max_num_ref_frames: u32,
    pub slice_mode: u32,
    pub slice_mode_data: u32,
}

/// NV_ENC_CODEC_CONFIG (union of the per-codec configs, 1792 bytes).
#[repr(C, align(8))]
#[derive(Clone, Copy)]
pub struct CodecConfig {
    pub raw: [u32; 448],
}

impl CodecConfig {
    pub fn h264(&mut self) -> &mut H264ConfigHead {
        // SAFETY: NV_ENC_CONFIG_H264 starts at offset 0 of the union with these u32 fields;
        // the head is smaller than the union and needs only 4-byte alignment.
        unsafe { &mut *(self.raw.as_mut_ptr() as *mut H264ConfigHead) }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Config {
    pub version: u32,
    pub profile_guid: Guid,
    pub gop_length: u32,
    pub frame_interval_p: i32,
    pub mono_chrome_encoding: u32,
    pub frame_field_mode: u32,
    pub mv_precision: u32,
    pub rc_params: RcParams,
    pub encode_codec_config: CodecConfig,
    pub reserved: [u32; 278],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct PresetConfig {
    pub version: u32,
    pub preset_cfg: Config,
    pub reserved1: [u32; 255],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct InitializeParams {
    pub version: u32,
    pub encode_guid: Guid,
    pub preset_guid: Guid,
    pub encode_width: u32,
    pub encode_height: u32,
    pub dar_width: u32,
    pub dar_height: u32,
    pub frame_rate_num: u32,
    pub frame_rate_den: u32,
    pub enable_encode_async: u32,
    pub enable_ptd: u32,
    pub flags: u32,
    pub priv_data_size: u32,
    pub priv_data: *mut c_void,
    pub encode_config: *mut Config,
    pub max_encode_width: u32,
    pub max_encode_height: u32,
    pub max_me_hint_counts_per_block: [[u32; 4]; 2],
    pub tuning_info: u32,
    pub buffer_format: u32,
    pub reserved: [u32; 287],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct CreateInputBuffer {
    pub version: u32,
    pub width: u32,
    pub height: u32,
    pub memory_heap: u32,
    pub buffer_fmt: u32,
    pub reserved: u32,
    pub input_buffer: InputPtr,
    pub p_sys_mem_buffer: *mut c_void,
    pub reserved1: [u32; 57],
    pub reserved2: [*mut c_void; 63],
}

#[repr(C)]
pub struct CreateBitstreamBuffer {
    pub version: u32,
    pub size: u32,
    pub memory_heap: u32,
    pub reserved: u32,
    pub bitstream_buffer: OutputPtr,
    pub bitstream_buffer_ptr: *mut c_void,
    pub reserved1: [u32; 58],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct LockInputBuffer {
    pub version: u32,
    pub flags: u32,
    pub input_buffer: InputPtr,
    pub buffer_data_ptr: *mut c_void,
    pub pitch: u32,
    pub reserved1: [u32; 251],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct LockBitstream {
    pub version: u32,
    pub flags: u32,
    pub output_bitstream: OutputPtr,
    pub slice_offsets: *mut u32,
    pub frame_idx: u32,
    pub hw_encode_status: u32,
    pub num_slices: u32,
    pub bitstream_size_in_bytes: u32,
    pub output_time_stamp: u64,
    pub output_duration: u64,
    pub bitstream_buffer_ptr: *mut c_void,
    pub picture_type: u32,
    pub picture_struct: u32,
    pub frame_avg_qp: u32,
    pub frame_satd: u32,
    pub ltr_frame_idx: u32,
    pub ltr_frame_bitmap: u32,
    pub temporal_id: u32,
    pub reserved: [u32; 12],
    pub intra_mb_count: u32,
    pub inter_mb_count: u32,
    pub average_mvx: i32,
    pub average_mvy: i32,
    pub alpha_layer_size_in_bytes: u32,
    pub reserved1: [u32; 218],
    pub reserved2: [*mut c_void; 64],
}

#[repr(C)]
pub struct PicParams {
    pub version: u32,
    pub input_width: u32,
    pub input_height: u32,
    pub input_pitch: u32,
    pub encode_pic_flags: u32,
    pub frame_idx: u32,
    pub input_time_stamp: u64,
    pub input_duration: u64,
    pub input_buffer: InputPtr,
    pub output_bitstream: OutputPtr,
    pub completion_event: *mut c_void,
    pub buffer_fmt: u32,
    pub picture_struct: u32,
    pub picture_type: u32,
    /// codecPicParams and everything after it (left zero)
    pub rest: [u64; 410],
}

#[repr(C)]
pub struct ReconfigureParams {
    pub version: u32,
    pub re_init_encode_params: InitializeParams,
    /// resetEncoder:1 forceIDR:1
    pub flags: u32,
}

/// All-zero value of a plain C struct (every field is an integer, pointer, array or Option<fn>).
pub fn zeroed<T>() -> Box<T> {
    // SAFETY: only used for the repr(C) structs above, for which all-zero bytes are valid
    // (null pointers / None function pointers / zero integers). Boxed: several are > 1 KB.
    unsafe { Box::new(std::mem::zeroed()) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // Values from the C header (clang, LP64/LLP64 x86_64 and arm64 identical).
    #[test]
    fn layouts_match_the_c_header() {
        assert_eq!(size_of::<FunctionList>(), 2552);
        assert_eq!(offset_of!(FunctionList, open_encode_session_ex), 240);
        assert_eq!(offset_of!(FunctionList, get_last_error_string), 304);
        assert_eq!(offset_of!(FunctionList, get_encode_preset_config_ex), 320);
        assert_eq!(size_of::<OpenEncodeSessionExParams>(), 1552);
        assert_eq!(offset_of!(OpenEncodeSessionExParams, api_version), 24);
        assert_eq!((size_of::<RcParams>(), align_of::<RcParams>()), (128, 4));
        assert_eq!(offset_of!(RcParams, average_bit_rate), 20);
        assert_eq!(offset_of!(RcParams, min_qp), 40);
        assert_eq!(offset_of!(RcParams, multi_pass), 100);
        assert_eq!(offset_of!(H264ConfigHead, idr_period), 8);
        assert_eq!(offset_of!(H264ConfigHead, entropy_coding_mode), 44);
        assert_eq!(offset_of!(H264ConfigHead, max_num_ref_frames), 60);
        assert_eq!((size_of::<CodecConfig>(), align_of::<CodecConfig>()), (1792, 8));
        assert_eq!(size_of::<Config>(), 3584);
        assert_eq!(offset_of!(Config, profile_guid), 4);
        assert_eq!(offset_of!(Config, gop_length), 20);
        assert_eq!(offset_of!(Config, rc_params), 40);
        assert_eq!(offset_of!(Config, encode_codec_config), 168);
        assert_eq!(offset_of!(Config, reserved), 1960);
        assert_eq!(offset_of!(Config, reserved2), 3072);
        assert_eq!(size_of::<PresetConfig>(), 5128);
        assert_eq!(offset_of!(PresetConfig, preset_cfg), 8);
        assert_eq!(offset_of!(PresetConfig, reserved1), 3592);
        assert_eq!(size_of::<InitializeParams>(), 1808);
        assert_eq!(offset_of!(InitializeParams, encode_width), 36);
        assert_eq!(offset_of!(InitializeParams, priv_data), 80);
        assert_eq!(offset_of!(InitializeParams, encode_config), 88);
        assert_eq!(offset_of!(InitializeParams, max_me_hint_counts_per_block), 104);
        assert_eq!(offset_of!(InitializeParams, tuning_info), 136);
        assert_eq!(offset_of!(InitializeParams, buffer_format), 140);
        assert_eq!(offset_of!(InitializeParams, reserved), 144);
        assert_eq!(size_of::<CreateInputBuffer>(), 776);
        assert_eq!(offset_of!(CreateInputBuffer, input_buffer), 24);
        assert_eq!(size_of::<CreateBitstreamBuffer>(), 776);
        assert_eq!(offset_of!(CreateBitstreamBuffer, bitstream_buffer), 16);
        assert_eq!(size_of::<LockInputBuffer>(), 1544);
        assert_eq!(offset_of!(LockInputBuffer, input_buffer), 8);
        assert_eq!(offset_of!(LockInputBuffer, pitch), 24);
        assert_eq!(size_of::<LockBitstream>(), 1544);
        assert_eq!(offset_of!(LockBitstream, output_bitstream), 8);
        assert_eq!(offset_of!(LockBitstream, bitstream_size_in_bytes), 36);
        assert_eq!(offset_of!(LockBitstream, output_time_stamp), 40);
        assert_eq!(offset_of!(LockBitstream, bitstream_buffer_ptr), 56);
        assert_eq!(offset_of!(LockBitstream, picture_type), 64);
        assert_eq!(size_of::<PicParams>(), 3360);
        assert_eq!(offset_of!(PicParams, input_time_stamp), 24);
        assert_eq!(offset_of!(PicParams, input_buffer), 40);
        assert_eq!(offset_of!(PicParams, buffer_fmt), 64);
        assert_eq!(offset_of!(PicParams, picture_type), 72);
        assert_eq!(offset_of!(PicParams, rest), 80);
        assert_eq!(size_of::<ReconfigureParams>(), 1824);
        assert_eq!(offset_of!(ReconfigureParams, re_init_encode_params), 8);
    }

    #[test]
    fn struct_versions_match_the_c_header() {
        assert_eq!(API_VERSION, 0xc);
        assert_eq!(FUNCTION_LIST_VER, 0x7002000c);
        assert_eq!(OPEN_ENCODE_SESSION_EX_PARAMS_VER, 0x7001000c);
        assert_eq!(PRESET_CONFIG_VER, 0xf004000c);
        assert_eq!(CONFIG_VER, 0xf008000c);
        assert_eq!(RC_PARAMS_VER, 0x7001000c);
        assert_eq!(INITIALIZE_PARAMS_VER, 0xf005000c);
        assert_eq!(CREATE_INPUT_BUFFER_VER, 0x7001000c);
        assert_eq!(CREATE_BITSTREAM_BUFFER_VER, 0x7001000c);
        assert_eq!(LOCK_INPUT_BUFFER_VER, 0x7001000c);
        assert_eq!(LOCK_BITSTREAM_VER, 0x7002000c);
        assert_eq!(PIC_PARAMS_VER, 0xf006000c);
        assert_eq!(RECONFIGURE_PARAMS_VER, 0xf001000c);
    }
}
