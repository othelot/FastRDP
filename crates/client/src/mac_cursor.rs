//! macOS: the remote pointer as a real OS cursor (`NSCursor`), so it moves with zero latency and
//! independently of video presentation - the way Windows App does it: an `NSCursor` from the
//! remote shape, sized by the points per remote pixel, set again on every mouse move over the
//! picture (Windows App: `MouseCursorView.cursorUpdate:` / `refreshCursor()` -> `[cursor set]`).
//!
//! winit keeps a cursor rectangle with its own pointer over the whole view, and AppKit re-applies
//! it on rectangle changes. While the mouse is over the picture the window's cursor rectangles are
//! therefore disabled; outside the picture (status bar, connection bar area off the picture, other
//! screens) they are enabled again and winit/iced decide - like `WM_SETCURSOR` on Windows.
//!
//! NSCursor cannot invert the screen: XOR pixels become black and a monochrome inverting shape
//! (the text I-beam) gets a one-pixel white halo, like Windows App, so it stays visible on dark
//! backgrounds. Other platforms composite the pointer in the video shader instead (video.rs).
use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::{AnyThread, MainThreadMarker};
use objc2_app_kit::{NSCursor, NSImage, NSView};
use objc2_core_foundation::CFData;
use objc2_core_graphics::{CGBitmapInfo, CGColorRenderingIntent, CGColorSpace, CGDataProvider, CGImage, CGImageAlphaInfo};
use objc2_foundation::{NSPoint, NSSize};
use protocol::CursorShape;

#[derive(Default)]
struct State {
    /// winit's content view
    view: Option<Retained<NSView>>,
    /// cursor shown over the picture; None = winit's pointer there too
    cursor: Option<Retained<NSCursor>>,
    /// (shape id, points per remote pixel * 1000) of `cursor`
    key: Option<(u32, u32)>,
    /// picture rectangle in view points, from the top left (x, y, w, h)
    picture: Option<(f64, f64, f64, f64)>,
    /// the window's cursor rectangles are disabled by us
    rects_off: bool,
}

thread_local! {
    // AppKit objects: main thread only (iced runs update/view there)
    static STATE: RefCell<State> = RefCell::new(State::default());
}

pub fn available() -> bool {
    STATE.with(|s| s.borrow().view.is_some())
}

/// Remembers winit's view (once). `ns_view` from raw-window-handle.
pub fn install(ns_view: isize) {
    if std::env::var_os("FASTRDP_NO_NATIVE_CURSOR").is_some() {
        return; // diagnostics: shader-drawn pointer only
    }
    if MainThreadMarker::new().is_none() || ns_view == 0 {
        return;
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.view.is_none() {
            // SAFETY: winit's NSView (raw-window-handle), alive as long as the window; retained here.
            s.view = unsafe { Retained::retain(ns_view as *mut NSView) };
        }
    });
}

/// RGBA of the remote shape for NSCursor (straight alpha), its size and hotspot: XOR pixels
/// (A=0, RGB!=0) become black; a monochrome inverting shape gets a white halo, one pixel larger
/// on every side.
fn cursor_pixels(shape: &CursorShape) -> (Vec<u8>, usize, usize, f64, f64) {
    let (w, h) = (shape.width as usize, shape.height as usize);
    let src = &shape.rgba[..w * h * 4];
    let xor = |p: &[u8]| p[3] == 0 && (p[0] | p[1] | p[2]) != 0;
    let has_xor = src.chunks_exact(4).any(xor);
    let mut px: Vec<u8> = src.chunks_exact(4).flat_map(|p| if xor(p) { [0, 0, 0, 255] } else { [p[0], p[1], p[2], p[3]] }).collect();
    let mono = src.chunks_exact(4).filter(|p| p[3] != 0).all(|p| p[0] == p[1] && p[1] == p[2]);
    if !(has_xor && mono) {
        return (px, w, h, shape.hot_x as f64, shape.hot_y as f64);
    }
    // halo: transparent pixels next to an opaque one become white
    let (hw, hh) = (w + 2, h + 2);
    let mut out = vec![0u8; hw * hh * 4];
    for y in 0..h {
        for x in 0..w {
            out[((y + 1) * hw + x + 1) * 4..][..4].copy_from_slice(&px[(y * w + x) * 4..][..4]);
        }
    }
    let solid = |o: &[u8], x: usize, y: usize| o[(y * hw + x) * 4 + 3] != 0;
    let snapshot = out.clone();
    for y in 0..hh {
        for x in 0..hw {
            if solid(&snapshot, x, y) {
                continue;
            }
            let near = (y.saturating_sub(1)..=(y + 1).min(hh - 1)).any(|ny| (x.saturating_sub(1)..=(x + 1).min(hw - 1)).any(|nx| solid(&snapshot, nx, ny)));
            if near {
                out[(y * hw + x) * 4..][..4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }
    }
    px = out;
    (px, hw, hh, shape.hot_x as f64 + 1.0, shape.hot_y as f64 + 1.0)
}

/// NSCursor for the remote shape at `points` per remote pixel (Retina: 0.5 at 1:1).
fn create_cursor(shape: &CursorShape, points: f64) -> Option<Retained<NSCursor>> {
    if shape.width == 0 || shape.height == 0 || shape.rgba.len() < shape.width as usize * shape.height as usize * 4 {
        return None;
    }
    let (rgba, w, h, hx, hy) = cursor_pixels(shape);
    let data = CFData::from_bytes(&rgba);
    let provider = CGDataProvider::with_cf_data(Some(&data))?;
    let space = CGColorSpace::new_device_rgb()?;
    // SAFETY: provider holds w*h*4 bytes of 8-bit RGBA rows.
    let image = unsafe {
        CGImage::new(
            w,
            h,
            8,
            32,
            w * 4,
            Some(&space),
            CGBitmapInfo(CGImageAlphaInfo::Last.0),
            Some(&provider),
            std::ptr::null(),
            false,
            CGColorRenderingIntent::RenderingIntentDefault,
        )
    }?;
    let size = NSSize::new(w as f64 * points, h as f64 * points);
    let ns_image = NSImage::initWithCGImage_size(NSImage::alloc(), &image, size);
    Some(NSCursor::initWithImage_hotSpot(NSCursor::alloc(), &ns_image, NSPoint::new(hx * points, hy * points)))
}

/// Current remote pointer (None/invisible = the local default pointer instead), the picture
/// rectangle in view points (x, y from the top left, w, h) and points per remote pixel. Call
/// from the UI thread after every update.
pub fn update(shape: Option<&CursorShape>, visible: bool, picture: Option<(f32, f32, f32, f32)>, points: f32) {
    if MainThreadMarker::new().is_none() {
        return;
    }
    STATE.with(|s| {
        let mut s = s.borrow_mut();
        if s.view.is_none() {
            return;
        }
        let active = picture.is_some() && visible && shape.is_some();
        if let (true, Some(shape)) = (active, shape) {
            let key = (shape.id, (points * 1000.0) as u32);
            if s.key != Some(key) {
                s.cursor = create_cursor(shape, points as f64);
                s.key = Some(key);
                if s.cursor.is_none() {
                    crate::log::line(&format!("native cursor: shape {} ({}x{}) could not be created", shape.id, shape.width, shape.height));
                }
            }
        } else {
            s.cursor = None;
            s.key = None;
        }
        s.picture = picture.map(|(x, y, w, h)| (x as f64, y as f64, w as f64, h as f64));
        apply(&mut s);
    });
}

/// The mouse moved (or entered / left the window): our cursor over the picture, winit's elsewhere.
pub fn refresh() {
    if MainThreadMarker::new().is_none() {
        return;
    }
    STATE.with(|s| {
        if let Ok(mut s) = s.try_borrow_mut() {
            apply(&mut s);
        }
    });
}

fn mouse_over_picture(s: &State) -> bool {
    let (Some(view), Some((x, y, w, h))) = (&s.view, s.picture) else { return false };
    let Some(window) = view.window() else { return false };
    let p = view.convertPoint_fromView(window.mouseLocationOutsideOfEventStream(), None);
    let top = if view.isFlipped() { p.y } else { view.bounds().size.height - p.y };
    p.x >= x && p.x < x + w && top >= y && top < y + h
}

fn apply(s: &mut State) {
    let Some(window) = s.view.as_ref().and_then(|v| v.window()) else { return };
    let ours = s.cursor.is_some() && mouse_over_picture(s);
    if ours {
        if !s.rects_off {
            window.disableCursorRects();
            s.rects_off = true;
        }
        if let Some(c) = &s.cursor {
            c.set();
        }
    } else if s.rects_off {
        // hand the pointer back to winit: its cursor rectangle applies again
        s.rects_off = false;
        window.enableCursorRects();
        if let Some(v) = &s.view {
            window.invalidateCursorRectsForView(v);
        }
        NSCursor::arrowCursor().set();
    }
}

#[cfg(test)]
mod tests {
    use protocol::CursorShape;

    #[test]
    fn xor_shapes_become_black_with_a_white_halo() {
        // 3x1: transparent, inverting (white XOR), transparent
        let shape = CursorShape { id: 1, width: 3, height: 1, hot_x: 1, hot_y: 0, rgba: [[0, 0, 0, 0], [255, 255, 255, 0], [0, 0, 0, 0]].concat() };
        let (px, w, h, hx, hy) = super::cursor_pixels(&shape);
        assert_eq!((w, h, hx, hy), (5, 3, 2.0, 1.0), "one pixel larger on every side, hotspot follows");
        let at = |x: usize, y: usize| &px[(y * w + x) * 4..][..4];
        assert_eq!(at(2, 1), [0, 0, 0, 255], "the inverting pixel is black");
        assert_eq!(at(1, 1), [255, 255, 255, 255], "halo");
        assert_eq!(at(2, 0), [255, 255, 255, 255], "halo above");
        assert_eq!(at(0, 0), [0, 0, 0, 0], "not next to the shape");
    }

    #[test]
    fn colour_shapes_are_unchanged() {
        let rgba = [[255, 0, 0, 255], [0, 0, 0, 0]].concat();
        let shape = CursorShape { id: 1, width: 2, height: 1, hot_x: 0, hot_y: 0, rgba: rgba.clone() };
        let (px, w, h, hx, hy) = super::cursor_pixels(&shape);
        assert_eq!((px, w, h, hx, hy), (rgba, 2, 1, 0.0, 0.0));
    }
}
