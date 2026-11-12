//! Remote display sizing: resolution choices for the connect screen and the mapping between the
//! client window (logical points) and the remote desktop (physical pixels, 1:1 when they match).
use iced::Size;
use protocol::DisplayRequest;

/// Height of the status bar under the picture in windowed mode (logical points).
pub const BAR_HEIGHT: f32 = 30.0;
/// Largest remote desktop the frame shared memory supports.
pub const MAX: (u32, u32) = (3840, 2160);

const PRESETS: &[(u32, u32)] = &[
    (1024, 768),
    (1280, 720),
    (1280, 800),
    (1366, 768),
    (1440, 900),
    (1600, 900),
    (1680, 1050),
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (2560, 1600),
    (3440, 1440),
    (3840, 2160),
];

/// The monitor the client window is on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    /// logical size (points)
    pub logical: Size,
    pub scale: f32,
}

impl Monitor {
    pub fn physical(&self) -> (u32, u32) {
        physical(self.logical, self.scale)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Choice {
    /// a window whose picture is exactly this many physical pixels
    Window(u32, u32),
    /// the whole monitor
    FullScreen,
}

/// Resolution slider positions: the presets that fit in a window on this monitor, then full screen.
pub fn choices(monitor: Option<Monitor>) -> Vec<Choice> {
    let (mw, mh) = monitor.map(|m| m.physical()).unwrap_or(MAX);
    let mut v: Vec<Choice> = PRESETS.iter().filter(|&&(w, h)| w <= mw && h < mh).map(|&(w, h)| Choice::Window(w, h)).collect();
    v.push(Choice::FullScreen);
    v
}

/// Remote desktop size for a choice (physical pixels, before the server-side clamp).
pub fn size_of(choice: Choice, monitor: Option<Monitor>) -> (u32, u32) {
    match choice {
        Choice::Window(w, h) => (w, h),
        Choice::FullScreen => monitor.map(|m| m.physical()).unwrap_or((1920, 1080)),
    }
}

pub fn label(choice: Choice, monitor: Option<Monitor>) -> String {
    match choice {
        Choice::Window(w, h) => format!("{w} × {h}"),
        Choice::FullScreen => {
            let (w, h) = size_of(choice, monitor);
            format!("Full screen ({w} × {h})")
        }
    }
}

pub fn physical(s: Size, scale: f32) -> (u32, u32) {
    ((s.width * scale).round().max(0.0) as u32, (s.height * scale).round().max(0.0) as u32)
}

/// Part of the window showing the remote desktop (logical points).
pub fn video_area(window: Size, fullscreen: bool) -> Size {
    if fullscreen { window } else { Size::new(window.width, (window.height - BAR_HEIGHT).max(1.0)) }
}

/// Window size (logical) whose picture area is exactly `w`×`h` physical pixels.
pub fn window_for(w: u32, h: u32, scale: f32) -> Size {
    Size::new(w as f32 / scale, h as f32 / scale + BAR_HEIGHT)
}

/// Display request for a picture area of `w`×`h` physical pixels at `scale` and `hz`, with a
/// bitrate target in kbit/s (0 = automatic).
pub fn request(w: u32, h: u32, scale: f32, hz: u32, bitrate_kbps: u32) -> DisplayRequest {
    DisplayRequest { width: w, height: h, refresh_hz: hz, scale_percent: (scale * 100.0).round() as u32, bitrate_kbps }.clamped(MAX.0, MAX.1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn choices_fit_the_monitor() {
        let m = Monitor { logical: Size::new(1706.67, 960.0), scale: 1.5 }; // 2560x1440 @ 150%
        assert_eq!(m.physical(), (2560, 1440));
        let c = choices(Some(m));
        assert_eq!(*c.last().unwrap(), Choice::FullScreen);
        assert!(c.contains(&Choice::Window(1920, 1080)));
        assert!(c.contains(&Choice::Window(2560, 1080)));
        assert!(!c.contains(&Choice::Window(2560, 1440)), "as tall as the monitor: use full screen");
        assert!(!c.contains(&Choice::Window(3840, 2160)));
        assert_eq!(label(Choice::FullScreen, Some(m)), "Full screen (2560 × 1440)");
        assert_eq!(size_of(Choice::FullScreen, None), (1920, 1080));
    }

    #[test]
    fn window_and_picture_are_one_to_one() {
        let w = window_for(1920, 1080, 1.5);
        assert_eq!(physical(video_area(w, false), 1.5), (1920, 1080));
        assert_eq!(physical(video_area(Size::new(1280.0, 720.0), true), 2.0), (2560, 1440));
        let r = request(1923, 1081, 1.25, 144, 0);
        assert_eq!((r.width, r.height, r.refresh_hz, r.scale_percent), (1920, 1080, 144, 125));
        assert_eq!(request(5120, 2880, 2.0, 60, 0).width, 3840);
        assert_eq!(request(1920, 1080, 1.0, 60, 40_000).bitrate_kbps, 40_000);
    }
}
