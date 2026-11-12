//! NVENC (NVIDIA hardware H.264) backend through the NVENCODE API, loaded at run time from the
//! driver (`nvEncodeAPI64.dll`, `nvcuda.dll`): nothing is linked at build time, and a machine
//! without an NVIDIA GPU / driver reports `Unavailable` (-> x264 fallback).
//!
//! * encode device: a CUDA context on CUDA device 0 (always the NVIDIA GPU, also on Optimus
//!   laptops where the DXGI default adapter is the integrated one);
//! * input: an NVENC-allocated ARGB buffer (= BGRA bytes) the frame is copied into; the RGB ->
//!   YUV conversion happens on the GPU;
//! * synchronous, P1..P7 preset with the ultra-low-latency tuning, CBR with a one-frame VBV,
//!   no B-frames, infinite GOP (IDR only on request), SPS/PPS repeated with every IDR;
//! * bitrate changes in place (`nvEncReconfigureEncoder`).
pub mod sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::Nvenc;
