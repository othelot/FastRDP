//! NVENC (NVIDIA hardware H.264) backend through the NVENCODE API, loaded at run time from the
//! driver (`nvEncodeAPI64.dll`, `nvcuda.dll`): nothing is linked at build time, and a machine
//! without an NVIDIA GPU / driver reports `Unavailable` (-> OpenH264 fallback).
//!
//! * encode device: a CUDA context on CUDA device 0 (always the NVIDIA GPU, also on Optimus
//!   laptops where the DXGI default adapter is the integrated one);
//! * input: an NVENC-allocated buffer the frame is copied into - IYUV when the display driver
//!   already converted it to I420 on the GPU, else ARGB (= BGRA bytes, converted by NVENC);
//! * synchronous, P1..P7 preset with the ultra-low-latency tuning, CBR with a one-frame VBV,
//!   no B-frames, infinite GOP (IDR only on request), SPS/PPS repeated with every IDR;
//! * bitrate changes in place (`nvEncReconfigureEncoder`);
//! * the H.264 level in the SPS is capped to 5.2 ([`cap_sps_level`]).
pub mod sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::Nvenc;

/// Highest H.264 level (`level_idc`) OpenH264, the client's decoder, accepts.
pub const MAX_LEVEL_IDC: u8 = 52;

/// Lowers `level_idc` of every SPS in an Annex-B access unit to [`MAX_LEVEL_IDC`].
///
/// NVENC picks the level from size and frame rate and writes 6.x for e.g. 3456x2160@120; OpenH264
/// rejects every 6.x stream (black picture), while a fixed level 5.2 makes NVENC refuse the
/// session ("Invalid Level"). The decoder only uses the level for its buffer sizes, so the byte is
/// rewritten. It is the third byte after the NAL header (profile_idc, constraint flags, level_idc);
/// profile_idc is never 0, so no emulation-prevention byte can sit before it.
pub fn cap_sps_level(au: &mut [u8]) {
    let mut i = 0;
    while i + 6 < au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            if au[i + 3] & 0x1f == 7 && au[i + 6] > MAX_LEVEL_IDC {
                au[i + 6] = MAX_LEVEL_IDC;
            }
            i += 3;
        } else {
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sps_level_is_capped() {
        // SPS (High, level 6.0) + PPS + IDR slice start
        let mut au = vec![0, 0, 0, 1, 0x67, 100, 0, 60, 0xAC, 0, 0, 0, 1, 0x68, 0xEE, 0x3C, 0x80, 0, 0, 1, 0x65, 0x88];
        let mut want = au.clone();
        want[7] = 52;
        super::cap_sps_level(&mut au);
        assert_eq!(au, want);
        let mut low = vec![0, 0, 1, 0x67, 66, 0xC0, 40, 0x8C];
        let before = low.clone();
        super::cap_sps_level(&mut low);
        assert_eq!(low, before, "levels up to 5.2 stay");
    }
}
