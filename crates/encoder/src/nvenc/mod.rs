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
//! * the rate control follows the frame intervals ([`rc_fps_for`]);
//! * the H.264 level in the SPS is capped to 5.2 ([`cap_sps_level`]).
pub mod sys;

#[cfg(windows)]
mod win;
#[cfg(windows)]
pub use win::Nvenc;

/// Lowest frame rate the rate control is told: a frame after a longer pause gets the bits of
/// 1/MIN_RC_FPS s (a large frame still has to cross the network in about that time).
pub const MIN_RC_FPS: u32 = 10;

/// Frame rate the rate control should assume for a frame `interval_us` after the previous one.
///
/// CBR gives every frame bitrate / frame rate. The encoder is set up for the session's refresh rate,
/// but frames only come when the screen changes: an app drawing 13 fps on a 240 Hz session got 1/240
/// of a second's bits per frame (1/18 of the bitrate; scrolling text fell apart). The rate is
/// stepped down by 1.5x from the configured one (few reconfigurations) to the step at or just above
/// the frame's own rate, never below MIN_RC_FPS: over time the stream uses the bitrate it was given.
pub fn rc_fps_for(configured: u32, interval_us: i64) -> u32 {
    let configured = configured.max(1);
    let floor = (1e6 / interval_us.max(1) as f64).max(MIN_RC_FPS as f64);
    let mut fps = configured as f64;
    while fps / 1.5 >= floor {
        fps /= 1.5;
    }
    (fps.round() as u32).clamp(1, configured)
}

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
    fn rate_control_follows_frame_intervals() {
        use super::rc_fps_for;
        assert_eq!(rc_fps_for(240, 4_167), 240, "frames at the refresh rate");
        assert_eq!(rc_fps_for(240, 16_667), 71, "60 fps app: next step above 60");
        assert_eq!(rc_fps_for(240, 76_923), 14, "13 fps console scroll");
        assert_eq!(rc_fps_for(240, 2_000_000), 14, "after a pause: not below MIN_RC_FPS");
        assert_eq!(rc_fps_for(60, 1_000), 60, "never above the configured rate");
        assert_eq!(rc_fps_for(60, 100_000), 12);
    }

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
