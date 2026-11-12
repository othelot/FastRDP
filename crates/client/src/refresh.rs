//! Refresh rate of the monitor the client window is on: the remote display is asked for the same
//! rate (high-refresh monitors get 120/144/240 Hz sessions). iced does not expose it, so the native
//! window handle (HWND / NSView) is used.

/// Fallback when the rate cannot be read.
pub const DEFAULT_HZ: u32 = 60;

/// Current refresh rate (Hz) of the monitor showing `window`, if it can be read.
#[cfg(windows)]
pub fn monitor_hz(window: isize) -> Option<u32> {
    use windows_sys::Win32::Graphics::Gdi::{
        DEVMODEW, ENUM_CURRENT_SETTINGS, EnumDisplaySettingsW, GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFOEXW, MonitorFromWindow,
    };
    // SAFETY: plain queries with sized out structs; the handle is our own top-level window.
    unsafe {
        let monitor = MonitorFromWindow(window as _, MONITOR_DEFAULTTONEAREST);
        if monitor.is_null() {
            return None;
        }
        let mut info: MONITORINFOEXW = std::mem::zeroed();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        if GetMonitorInfoW(monitor, &mut info as *mut MONITORINFOEXW as *mut _) == 0 {
            return None;
        }
        let mut mode: DEVMODEW = std::mem::zeroed();
        mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) == 0 {
            return None;
        }
        // 0 and 1 mean "hardware default"
        (mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
    }
}

/// macOS: `maximumFramesPerSecond` of the screen the window is on (ProMotion: 120). `window` is
/// winit's NSView; AppKit, so main thread only (iced runs update there).
#[cfg(target_os = "macos")]
pub fn monitor_hz(window: isize) -> Option<u32> {
    use objc2_app_kit::NSView;
    objc2::MainThreadMarker::new()?;
    if window == 0 {
        return None;
    }
    // SAFETY: winit's NSView from raw-window-handle, alive as long as the window; only read here.
    let view: &NSView = unsafe { &*(window as *const NSView) };
    let hz = view.window()?.screen()?.maximumFramesPerSecond();
    u32::try_from(hz).ok().filter(|&hz| hz > 1)
}

#[cfg(not(any(windows, target_os = "macos")))]
pub fn monitor_hz(_window: isize) -> Option<u32> {
    None
}

/// Rate to request: the monitor's, limited to what the protocol accepts (24..=240), else 60.
pub fn request_hz(window: Option<isize>) -> u32 {
    window.and_then(monitor_hz).filter(|hz| (24..=240).contains(hz)).unwrap_or(DEFAULT_HZ)
}

#[cfg(test)]
mod tests {
    #[test]
    fn unknown_or_out_of_range_rates_fall_back_to_60() {
        assert_eq!(super::request_hz(None), 60);
    }
}
