//! Windows: the remote pointer as a real OS cursor (like mstsc), so it moves with zero latency and
//! independently of video presentation. The window is subclassed; on `WM_SETCURSOR` over the
//! picture our `HCURSOR` (built from the remote shape) is used, everywhere else winit/iced decide.
//! Other platforms composite the pointer in the video shader instead (video.rs).
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use protocol::CursorShape;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateBitmap, CreateDIBSection, DIB_RGB_COLORS, DeleteObject, GetDC, ReleaseDC, ScreenToClient,
};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, DestroyCursor, GetCursorPos, HCURSOR, HTCLIENT, ICONINFO, SetCursor, WM_SETCURSOR};

const SUBCLASS_ID: usize = 0x4652_4450; // "FRDP"

struct State {
    hwnd: HWND,
    /// picture rectangle in client pixels (x, y, w, h); None = not on the remote screen
    picture: Option<(i32, i32, i32, i32)>,
    /// part of the picture covered by local controls (the full-screen connection bar)
    excluded: Option<(i32, i32, i32, i32)>,
    /// (shape id, scaled size) of `cursor`
    key: Option<(u32, u32)>,
    cursor: HCURSOR,
}

// SAFETY: plain handles, used on the window thread under the mutex.
unsafe impl Send for State {}

static STATE: Mutex<State> = Mutex::new(State { hwnd: std::ptr::null_mut(), picture: None, excluded: None, key: None, cursor: std::ptr::null_mut() });
static ACTIVE: AtomicBool = AtomicBool::new(false);
static CURRENT: AtomicIsize = AtomicIsize::new(0);

pub fn available() -> bool {
    !STATE.lock().unwrap().hwnd.is_null()
}

/// Subclasses the window (once). `hwnd` from raw-window-handle.
pub fn install(hwnd: isize) {
    if std::env::var_os("FASTRDP_NO_NATIVE_CURSOR").is_some() {
        return; // diagnostics: shader-drawn pointer only
    }
    let mut s = STATE.lock().unwrap();
    if !s.hwnd.is_null() {
        return;
    }
    s.hwnd = hwnd as HWND;
    // SAFETY: valid top-level window of this thread's event loop.
    unsafe { SetWindowSubclass(s.hwnd, Some(subclass_proc), SUBCLASS_ID, 0) };
}

fn over_picture(s: &State) -> bool {
    let mut p = POINT { x: 0, y: 0 };
    // SAFETY: valid out pointer and window.
    unsafe {
        GetCursorPos(&mut p);
        ScreenToClient(s.hwnd, &mut p);
    }
    let inside = |r: Option<(i32, i32, i32, i32)>| r.is_some_and(|(x, y, w, h)| p.x >= x && p.y >= y && p.x < x + w && p.y < y + h);
    inside(s.picture) && !inside(s.excluded)
}

unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_SETCURSOR
        && (lparam & 0xFFFF) as u32 == HTCLIENT
        && ACTIVE.load(Ordering::Acquire)
        && let Ok(s) = STATE.try_lock()
        && over_picture(&s)
    {
        // SAFETY: cursor handle owned by STATE (null = hidden pointer).
        unsafe { SetCursor(CURRENT.load(Ordering::Acquire) as HCURSOR) };
        return 1;
    }
    // SAFETY: forwarding to winit's window procedure.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}

/// Builds an HCURSOR: alpha cursors as 32-bit ARGB; shapes with XOR pixels (A=0, RGB!=0) as
/// classic AND/XOR masks so Windows inverts the screen like on the remote side.
fn create_cursor(shape: &CursorShape, scale: f32) -> HCURSOR {
    let (sw, sh) = (shape.width as usize, shape.height as usize);
    let (w, h) = (((sw as f32 * scale).round() as usize).clamp(1, 256), ((sh as f32 * scale).round() as usize).clamp(1, 256));
    let px = |x: usize, y: usize| -> [u8; 4] {
        let (ox, oy) = ((x * sw / w).min(sw - 1), (y * sh / h).min(sh - 1));
        shape.rgba[(oy * sw + ox) * 4..][..4].try_into().unwrap()
    };
    let has_xor = shape.rgba.chunks_exact(4).any(|p| p[3] == 0 && (p[0] | p[1] | p[2]) != 0);
    // SAFETY: GDI objects created here are released after CreateIconIndirect copied them.
    unsafe {
        let bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..std::mem::zeroed()
            },
            bmiColors: [std::mem::zeroed()],
        };
        let dc = GetDC(std::ptr::null_mut());
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let color = CreateDIBSection(dc, &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
        ReleaseDC(std::ptr::null_mut(), dc);
        if color.is_null() || bits.is_null() {
            return std::ptr::null_mut();
        }
        let out = std::slice::from_raw_parts_mut(bits as *mut u8, w * h * 4);
        // monochrome AND mask, rows padded to 16 bits
        let stride = w.div_ceil(16) * 2;
        let mut mask = vec![0u8; stride * h];
        for y in 0..h {
            for x in 0..w {
                let [r, g, b, a] = px(x, y);
                let o = &mut out[(y * w + x) * 4..][..4];
                if has_xor {
                    // screen = (screen AND mask) XOR color
                    let transparent_or_xor = a == 0;
                    if transparent_or_xor {
                        mask[y * stride + x / 8] |= 0x80 >> (x % 8);
                    }
                    o.copy_from_slice(&[b, g, r, 0]);
                } else {
                    o.copy_from_slice(&[b, g, r, a]);
                }
            }
        }
        let and = CreateBitmap(w as i32, h as i32, 1, 1, mask.as_ptr() as *const _);
        let hot_x = (shape.hot_x as f32 * w as f32 / sw as f32) as u32;
        let hot_y = (shape.hot_y as f32 * h as f32 / sh as f32) as u32;
        let ii = ICONINFO { fIcon: 0, xHotspot: hot_x, yHotspot: hot_y, hbmMask: and, hbmColor: color };
        let c = CreateIconIndirect(&ii);
        DeleteObject(and);
        DeleteObject(color);
        c
    }
}

/// Current remote pointer (`visible`: None = nothing known yet), the picture rectangle and the part
/// of it covered by local controls, in client pixels, and the remote->window pixel scale. Call from
/// the UI thread.
///
/// A remote pointer reported hidden is hidden over the picture too when `hide_hidden` (like mstsc
/// with the RDP null pointer): Windows also reports it hidden while it draws the pointer into the
/// picture itself (software cursor), and a local arrow on top of that made two pointers. Without
/// `hide_hidden` (input through the agent: the session may have no mouse device, so Windows hides
/// the pointer for good), or while nothing is known, the local default pointer is shown instead.
pub fn update(
    shape: Option<&CursorShape>,
    visible: Option<bool>,
    picture: Option<(i32, i32, i32, i32)>,
    excluded: Option<(i32, i32, i32, i32)>,
    scale: f32,
    hide_hidden: bool,
) {
    let mut s = STATE.lock().unwrap();
    if s.hwnd.is_null() {
        return;
    }
    let moved = s.picture != picture || s.excluded != excluded;
    s.picture = picture;
    s.excluded = excluded;
    let before = CURRENT.load(Ordering::Acquire);
    if let (Some(_), Some(true), Some(shape)) = (picture, visible, shape) {
        let key = (shape.id, (scale * 1000.0) as u32);
        if s.key != Some(key) {
            let c = create_cursor(shape, scale);
            if !c.is_null() {
                let old = std::mem::replace(&mut s.cursor, c);
                if CURRENT.load(Ordering::Acquire) == old as isize {
                    CURRENT.store(c as isize, Ordering::Release);
                }
                s.key = Some(key);
                // SAFETY: old cursor no longer referenced (CURRENT moved off it first).
                if !old.is_null() {
                    unsafe { DestroyCursor(old) };
                }
            }
        }
    }
    // the pointer over the picture: the remote shape, nothing (hidden), or winit's (inactive)
    let want = match (picture, visible) {
        (Some(_), Some(true)) if shape.is_some() && !s.cursor.is_null() => Some(s.cursor),
        (Some(_), Some(false)) if hide_hidden => Some(std::ptr::null_mut()),
        _ => None,
    };
    let now_active = want.is_some();
    let cursor = want.unwrap_or(std::ptr::null_mut());
    CURRENT.store(cursor as isize, Ordering::Release);
    let was = ACTIVE.swap(now_active, Ordering::AcqRel);
    if was == now_active && (!now_active || (before == cursor as isize && !moved)) {
        return; // nothing changed: leave the pointer to WM_SETCURSOR
    }
    // apply immediately (shape change without a mouse move); else winit's next WM_SETCURSOR
    if now_active && over_picture(&s) {
        // SAFETY: valid cursor handle or null (hidden), UI thread.
        unsafe { SetCursor(cursor) };
    } else if (was && !now_active) || (now_active && moved) {
        // no longer ours here (inactive, or the picture / connection bar moved under the pointer)
        // hand the pointer back to winit (it re-applies its cursor on the next WM_SETCURSOR)
        // SAFETY: trivial.
        unsafe {
            SetCursor(windows_sys::Win32::UI::WindowsAndMessaging::LoadCursorW(std::ptr::null_mut(), windows_sys::Win32::UI::WindowsAndMessaging::IDC_ARROW))
        };
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn creates_alpha_and_xor_cursors() {
        let alpha = protocol::CursorShape { id: 1, width: 4, height: 4, hot_x: 1, hot_y: 1, rgba: [255, 0, 0, 200].repeat(16) };
        let c = super::create_cursor(&alpha, 1.5);
        assert!(!c.is_null());
        let mut xor = [0u8, 0, 0, 0].repeat(16);
        xor[0..4].copy_from_slice(&[255, 255, 255, 0]);
        let c2 = super::create_cursor(&protocol::CursorShape { rgba: xor, ..alpha }, 1.0);
        assert!(!c2.is_null());
        // SAFETY: created above.
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyCursor(c);
            windows_sys::Win32::UI::WindowsAndMessaging::DestroyCursor(c2);
        }
    }
}
