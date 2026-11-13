//! Windows: "apply Windows key combinations on the remote computer", built like mstsc
//! (mstscax.dll, input handler window):
//!
//! - a low-level keyboard hook is installed once, on a dedicated thread with its own message loop;
//! - for every key the hook decides on the spot: capture only if the foreground window belongs to
//!   this process AND the keyboard focus of our UI thread is our window (and capturing is enabled:
//!   remote screen + option);
//! - a captured key is handed to the session input channel and swallowed (return 1); everything
//!   else goes on with `CallNextHookEx`.
//!
//! winit registers the keyboard for Raw Input (WM_INPUT, `RIDEV_DEVNOTIFY`). While a window of a
//! process with a Raw Input keyboard registration is in front, Windows does not call that
//! process's own low-level keyboard hook for key presses (reproduced with a plain WinForms window:
//! with the registration Alt/Tab presses never reach the hook, without it they do). iced does not use winit's raw keyboard events, so the keyboard registration is removed
//! (the mouse one stays). mstsc has no Raw Input keyboard registration.
//!
//! Ctrl+Alt+Del cannot be hooked (secure attention sequence): Ctrl+Alt+End sends it instead.
//! Ctrl+Alt+Break stays local (full-screen toggle). AltGr's fake LCtrl is ignored.
use std::collections::BTreeSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};

use protocol::{InputEvent, SCANCODE_PAUSE};
use tokio::sync::mpsc::UnboundedSender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{VK_CANCEL, VK_HANGUL, VK_HANJA, VK_PAUSE, VK_RSHIFT};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetWindowThreadProcessId, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_UP, SetWindowsHookExW, WH_KEYBOARD_LL,
};

use crate::session::InputCmd;

struct State {
    hwnd: HWND,
    input: Option<UnboundedSender<InputCmd>>,
    /// remote screen shown and the option says "remote" (focus is checked per key)
    enabled: bool,
    down: BTreeSet<(u16, bool)>,
}

// SAFETY: plain window handle, only compared.
unsafe impl Send for State {}

static STATE: Mutex<State> = Mutex::new(State { hwnd: std::ptr::null_mut(), input: None, enabled: false, down: BTreeSet::new() });
static INSTALLED: Mutex<bool> = Mutex::new(false);
static UI_THREAD: AtomicU32 = AtomicU32::new(0);

/// Keyboard focus window of the UI thread.
fn focus_of_ui_thread() -> HWND {
    use windows_sys::Win32::UI::WindowsAndMessaging::{GUITHREADINFO, GetGUIThreadInfo};
    // SAFETY: plain C struct, zero is valid.
    let mut gi: GUITHREADINFO = unsafe { std::mem::zeroed() };
    gi.cbSize = std::mem::size_of::<GUITHREADINFO>() as u32;
    // SAFETY: sized out struct.
    if unsafe { GetGUIThreadInfo(UI_THREAD.load(Relaxed), &mut gi) } == 0 {
        return std::ptr::null_mut();
    }
    gi.hwndFocus
}

/// Removes this process's Raw Input keyboard registration (made by winit); see the module docs.
fn remove_raw_keyboard() -> bool {
    use windows_sys::Win32::UI::Input::{RAWINPUTDEVICE, RIDEV_REMOVE, RegisterRawInputDevices};
    // generic desktop / keyboard; RIDEV_REMOVE requires a null target
    let d = RAWINPUTDEVICE { usUsagePage: 1, usUsage: 6, dwFlags: RIDEV_REMOVE, hwndTarget: std::ptr::null_mut() };
    // SAFETY: one sized struct.
    unsafe { RegisterRawInputDevices(&d, 1, std::mem::size_of::<RAWINPUTDEVICE>() as u32) != 0 }
}

/// Installs the hook once (call with our top-level window, on the UI thread, after winit set up
/// its Raw Input registration).
pub fn install(hwnd: isize) {
    STATE.lock().unwrap().hwnd = hwnd as HWND;
    let mut done = INSTALLED.lock().unwrap();
    if *done {
        return;
    }
    *done = true;
    let raw_removed = remove_raw_keyboard();
    // SAFETY: valid window handle.
    UI_THREAD.store(unsafe { GetWindowThreadProcessId(hwnd as HWND, std::ptr::null_mut()) }, Relaxed);
    let spawned = std::thread::Builder::new().name("fastrdp-keyhook".into()).spawn(move || {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL};
        use windows_sys::Win32::UI::WindowsAndMessaging::{DispatchMessageW, GetMessageW, MSG, TranslateMessage};
        // SAFETY: hook + message loop owned by this thread for the process lifetime.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let hook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), GetModuleHandleW(std::ptr::null()), 0);
            crate::log::line(&format!("keyhook: installed={}, raw keyboard registration removed={raw_removed}", !hook.is_null()));
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    });
    if let Err(e) = spawned {
        crate::log::line(&format!("keyhook: thread: {e}"));
    }
}

/// Capturing allowed right now (remote screen + "Windows key combinations" option), and where
/// captured keys go. Whether our window has the focus is decided per key in the hook.
pub fn configure(input: Option<UnboundedSender<InputCmd>>, enabled: bool) {
    let mut s = STATE.lock().unwrap();
    let now = enabled && input.is_some();
    if s.enabled && !now && !s.down.is_empty() {
        release_all(&mut s);
    }
    s.enabled = now;
    s.input = input;
}

fn release_all(s: &mut State) {
    if let Some(i) = &s.input {
        let _ = i.send(InputCmd::Event(InputEvent::ReleaseAll));
    }
    s.down.clear();
}

/// LL hook data -> (scancode, extended) in the protocol's hardware encoding.
fn scancode(k: &KBDLLHOOKSTRUCT) -> Option<(u16, bool)> {
    let ext = k.flags & LLKHF_EXTENDED != 0;
    Some(match k.vkCode as u16 {
        VK_PAUSE => (SCANCODE_PAUSE, false),
        VK_HANGUL => (0xF2, false),
        VK_HANJA => (0xF1, false),
        // NumLock is reported with the extended flag; on the wire it is plain 0x45
        _ if k.scanCode == 0x45 => (0x45, false),
        // so is Right Shift; E0 36 is the "fake shift" keyboards send around other keys, which
        // Windows ignores - the session never saw Right Shift
        VK_RSHIFT => (0x36, false),
        _ if k.scanCode == 0 || k.scanCode > 0xFF => return None,
        _ => (k.scanCode as u16, ext),
    })
}

/// What to do with one key while capturing: None = leave it to the local system,
/// Some((scancode, extended, down)) = send it to the session and swallow it. Updates `down`.
fn decide(down: &mut BTreeSet<(u16, bool)>, k: &KBDLLHOOKSTRUCT) -> Option<(u16, bool, bool)> {
    if k.scanCode & 0x200 != 0 {
        return None; // the fake LCtrl of AltGr (mstsc skips vk 0xA2 / scancode 0x21D too)
    }
    let up = k.flags & LLKHF_UP != 0;
    let ctrl = down.contains(&(0x1D, false)) || down.contains(&(0x1D, true));
    let alt = down.contains(&(0x38, false)) || down.contains(&(0x38, true));
    // Ctrl+Alt+Break (VK_CANCEL with Ctrl) stays local: full-screen toggle in the window
    if k.vkCode as u16 == VK_CANCEL || (k.vkCode as u16 == VK_PAUSE && ctrl && alt) {
        return None;
    }
    let mut sc = scancode(k)?;
    // Ctrl+Alt+End -> Ctrl+Alt+Del on the session
    if sc == (0x4F, true) && ctrl && alt {
        sc = (0x53, true);
    }
    if up {
        down.remove(&sc);
    } else {
        down.insert(sc);
    }
    Some((sc.0, sc.1, !up))
}

/// Counts only (keys sent to the session, left local, send failures) - which keys were pressed is
/// never recorded anywhere.
static STATS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];

pub fn stats() -> (u32, u32, u32) {
    (STATS[0].load(Relaxed), STATS[1].load(Relaxed), STATS[2].load(Relaxed))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // SAFETY: lparam points to a KBDLLHOOKSTRUCT for HC_ACTION.
    let next = || unsafe { CallNextHookEx(std::ptr::null_mut::<std::ffi::c_void>() as HHOOK, code, wparam, lparam) };
    if code < 0 {
        return next();
    }
    // never let a panic unwind into the system (that would abort the process)
    let handled = std::panic::catch_unwind(|| {
        let k = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        let mut s = STATE.lock().unwrap();
        // mstsc: foreground window of this process AND keyboard focus on our window
        // SAFETY: trivial query.
        let fg = unsafe { GetForegroundWindow() };
        let focus = focus_of_ui_thread();
        let mut pid = 0u32;
        // SAFETY: valid out pointer.
        unsafe { GetWindowThreadProcessId(fg, &mut pid) };
        let ours = pid == std::process::id() && !focus.is_null() && focus == s.hwnd;
        if !s.enabled || !ours {
            if !s.down.is_empty() {
                release_all(&mut s);
            }
            return false;
        }
        let st = &mut *s;
        let Some((scancode, extended, down)) = decide(&mut st.down, k) else {
            return false;
        };
        let ok = st.input.as_ref().is_some_and(|i| i.send(InputCmd::Event(InputEvent::Key { scancode, extended, down })).is_ok());
        STATS[if ok { 0 } else { 2 }].fetch_add(1, Relaxed);
        true
    });
    match handled {
        Ok(true) => 1, // keep it from the local system
        Ok(false) => {
            STATS[1].fetch_add(1, Relaxed);
            next()
        }
        Err(_) => next(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn k(vk: u16, sc: u32, flags: u32) -> KBDLLHOOKSTRUCT {
        KBDLLHOOKSTRUCT { vkCode: vk as u32, scanCode: sc, flags, time: 0, dwExtraInfo: 0 }
    }

    #[test]
    fn hardware_encoding() {
        assert_eq!(scancode(&k(0x5B, 0x5B, LLKHF_EXTENDED)), Some((0x5B, true)), "left Windows key");
        assert_eq!(scancode(&k(0x09, 0x0F, 0)), Some((0x0F, false)), "Tab");
        assert_eq!(scancode(&k(0x90, 0x45, LLKHF_EXTENDED)), Some((0x45, false)), "NumLock");
        assert_eq!(scancode(&k(VK_PAUSE, 0x45, 0)), Some((SCANCODE_PAUSE, false)));
        assert_eq!(scancode(&k(VK_RSHIFT, 0x36, LLKHF_EXTENDED)), Some((0x36, false)), "Right Shift");
        assert_eq!(scancode(&k(0xA0, 0x2A, 0)), Some((0x2A, false)), "Left Shift");
        assert_eq!(scancode(&k(VK_HANGUL, 0x72, 0)), Some((0xF2, false)));
        assert_eq!(scancode(&k(0xFF, 0, 0)), None);
    }

    fn run(seq: &[(u16, u32, u32)]) -> Vec<Option<(u16, bool, bool)>> {
        let mut down = BTreeSet::new();
        seq.iter().map(|&(vk, sc, fl)| decide(&mut down, &k(vk, sc, fl))).collect()
    }

    #[test]
    fn combinations_go_to_the_session() {
        const E: u32 = LLKHF_EXTENDED;
        const U: u32 = LLKHF_UP;
        // Win+R
        assert_eq!(
            run(&[(0x5B, 0x5B, E), (0x52, 0x13, 0), (0x52, 0x13, U), (0x5B, 0x5B, E | U)]),
            vec![Some((0x5B, true, true)), Some((0x13, false, true)), Some((0x13, false, false)), Some((0x5B, true, false))]
        );
        // Alt+Tab
        assert_eq!(run(&[(0x12, 0x38, 0), (0x09, 0x0F, 0)]), vec![Some((0x38, false, true)), Some((0x0F, false, true))]);
        // Ctrl+Alt+End -> Ctrl+Alt+Del
        let r = run(&[(0x11, 0x1D, 0), (0x12, 0x38, 0), (0x23, 0x4F, E), (0x23, 0x4F, E | U)]);
        assert_eq!(r[2], Some((0x53, true, true)));
        // Ctrl+Alt+Break stays local
        assert_eq!(run(&[(0x11, 0x1D, 0), (0x12, 0x38, 0), (VK_CANCEL, 0x46, E)])[2], None);
        // AltGr's fake LCtrl stays out, the right Alt goes
        assert_eq!(run(&[(0x11, 0x21D, 0), (0x12, 0x38, E)]), vec![None, Some((0x38, true, true))]);
    }
}
