//! macOS: "apply Windows key combinations on the remote computer" (Cmd+Tab, Cmd+Space, Ctrl+arrows,
//! Cmd+Q, ...), the counterpart of `keyhook.rs`:
//!
//! - a Quartz event tap at the HID level (before the window server hands shortcuts to the Dock,
//!   Spotlight or the app menu) runs on a dedicated thread with its own run loop;
//! - while our window has the focus and capturing is enabled, every key is sent to the session
//!   and swallowed; otherwise it goes on unchanged;
//! - Caps Lock always stays local (the toggle is a hardware state) and the session's lock keys are
//!   set to match it instead.
//!
//! An event tap that can swallow keys needs the Accessibility permission (System Settings >
//! Privacy & Security > Accessibility). The tap is created only when capturing is first wanted;
//! without the permission keys go through the window as before (system shortcuts stay local).
use std::cell::RefCell;
use std::collections::BTreeSet;
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use objc2_core_foundation::{CFMachPort, CFRetained, CFRunLoop, kCFRunLoopCommonModes};
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventTapLocation, CGEventTapOptions, CGEventTapPlacement, CGEventTapProxy,
    CGEventType, CGPreflightPostEventAccess, CGRequestPostEventAccess,
};
use protocol::InputEvent;
use tokio::sync::mpsc::UnboundedSender;

use crate::session::InputCmd;

const KC_CAPS_LOCK: u16 = 0x39;

struct State {
    input: Option<UnboundedSender<InputCmd>>,
    /// remote screen shown and the option says "remote"
    enabled: bool,
    /// our window has the keyboard focus
    focused: bool,
    down: BTreeSet<(u16, bool)>,
}

static STATE: Mutex<State> = Mutex::new(State { input: None, enabled: false, focused: false, down: BTreeSet::new() });
/// tap thread running (or being started)
static RUNNING: AtomicBool = AtomicBool::new(false);

thread_local! {
    /// the tap, on its own thread (re-enabled there after a timeout)
    static TAP: RefCell<Option<CFRetained<CFMachPort>>> = const { RefCell::new(None) };
}

/// Capturing allowed right now (remote screen + "Windows key combinations" option), whether our
/// window has the focus, and where captured keys go. Call after every UI update.
pub fn configure(input: Option<UnboundedSender<InputCmd>>, enabled: bool, focused: bool) {
    let mut s = STATE.lock().unwrap();
    let now = enabled && input.is_some();
    if (!now || !focused) && !s.down.is_empty() {
        release_all(&mut s);
    }
    let start = now && !s.enabled;
    s.enabled = now;
    s.focused = focused;
    s.input = input;
    drop(s);
    if start {
        start_tap();
    }
}

/// Creates the tap on its own thread, once it is running; a failed attempt (no permission yet)
/// is retried the next time capturing is switched on (next connection).
fn start_tap() {
    if RUNNING.swap(true, Relaxed) {
        return;
    }
    let spawned = std::thread::Builder::new().name("fastrdp-keytap".into()).spawn(|| {
        let trusted = CGPreflightPostEventAccess();
        if !trusted {
            // opens the system prompt (once) pointing to the Accessibility settings
            CGRequestPostEventAccess();
        }
        let mask = (1u64 << CGEventType::KeyDown.0) | (1u64 << CGEventType::KeyUp.0) | (1u64 << CGEventType::FlagsChanged.0);
        // SAFETY: the callback matches CGEventTapCallBack and ignores user_info.
        let tap = unsafe {
            CGEvent::tap_create(
                CGEventTapLocation::HIDEventTap,
                CGEventTapPlacement::HeadInsertEventTap,
                CGEventTapOptions::Default,
                mask,
                Some(tap_proc),
                std::ptr::null_mut(),
            )
        };
        let Some(tap) = tap else {
            crate::log::line(&format!("keytap: not installed (accessibility permission: {trusted}); system shortcuts stay local"));
            RUNNING.store(false, Relaxed);
            return;
        };
        let Some(source) = CFMachPort::new_run_loop_source(None, Some(&tap), 0) else {
            crate::log::line("keytap: no run loop source");
            RUNNING.store(false, Relaxed);
            return;
        };
        let Some(run_loop) = CFRunLoop::current() else {
            RUNNING.store(false, Relaxed);
            return;
        };
        // SAFETY: immutable static provided by CoreFoundation.
        run_loop.add_source(Some(&source), unsafe { kCFRunLoopCommonModes });
        CGEvent::tap_enable(&tap, true);
        TAP.with(|t| *t.borrow_mut() = Some(tap));
        crate::log::line("keytap: installed");
        CFRunLoop::run();
    });
    if let Err(e) = spawned {
        crate::log::line(&format!("keytap: thread: {e}"));
        RUNNING.store(false, Relaxed);
    }
}

fn release_all(s: &mut State) {
    if let Some(i) = &s.input {
        let _ = i.send(InputCmd::Event(InputEvent::ReleaseAll));
    }
    s.down.clear();
}

/// Session lock keys for a local Caps Lock state. Mac keyboards have no Num Lock (the keypad
/// always types digits) and no Scroll Lock.
pub fn leds(caps: bool) -> u8 {
    protocol::leds::NUM_LOCK | if caps { protocol::leds::CAPS_LOCK } else { 0 }
}

/// Local Caps Lock state as session lock keys.
pub fn local_leds() -> u8 {
    leds(CGEventSource::flags_state(CGEventSourceStateID::HIDSystemState).contains(CGEventFlags::MaskAlphaShift))
}

/// Device-dependent modifier bits (IOKit NX_DEVICE*KEYMASK) of the modifier keys, so a
/// flagsChanged event tells which side was pressed or released.
fn modifier_bit(kc: u16) -> Option<u64> {
    Some(match kc {
        0x3B => 0x0001, // left Control
        0x38 => 0x0002, // left Shift
        0x3C => 0x0004, // right Shift
        0x37 => 0x0008, // left Command
        0x36 => 0x0010, // right Command
        0x3A => 0x0020, // left Option
        0x3D => 0x0040, // right Option
        0x3E => 0x2000, // right Control
        _ => return None,
    })
}

/// What to do with one key while capturing: None = leave it to the local system,
/// Some((scancode, extended, down)) = send it to the session and swallow it. Updates `down`.
fn decide(down: &mut BTreeSet<(u16, bool)>, ty: CGEventType, kc: u16, flags: u64) -> Option<(u16, bool, bool)> {
    let pressed = match ty {
        CGEventType::KeyDown => true,
        CGEventType::KeyUp => false,
        CGEventType::FlagsChanged => flags & modifier_bit(kc)? != 0,
        _ => return None,
    };
    let mut sc = crate::keymap::mac_keycode(kc)?;
    let ctrl = down.contains(&(0x1D, false)) || down.contains(&(0x1D, true));
    let alt = down.contains(&(0x38, false)) || down.contains(&(0x38, true));
    // Ctrl+Option+End (Fn+Right on laptops) -> Ctrl+Alt+Del on the session
    if sc == (0x4F, true) && ctrl && alt {
        sc = (0x53, true);
    }
    if pressed {
        down.insert(sc);
    } else {
        down.remove(&sc);
    }
    Some((sc.0, sc.1, pressed))
}

/// Counts only (keys sent to the session, left local, send failures) - which keys were pressed is
/// never recorded anywhere.
static STATS: [AtomicU32; 3] = [const { AtomicU32::new(0) }; 3];

pub fn stats() -> (u32, u32, u32) {
    (STATS[0].load(Relaxed), STATS[1].load(Relaxed), STATS[2].load(Relaxed))
}

/// true = swallow the event.
fn handle(ty: CGEventType, ev: &CGEvent) -> bool {
    let kc = CGEvent::integer_value_field(Some(ev), CGEventField::KeyboardEventKeycode) as u16;
    let flags = CGEvent::flags(Some(ev));
    let mut s = STATE.lock().unwrap();
    if !s.focused || s.input.is_none() {
        if !s.down.is_empty() {
            release_all(&mut s);
        }
        return false;
    }
    if kc == KC_CAPS_LOCK {
        if ty == CGEventType::FlagsChanged
            && let Some(i) = &s.input
        {
            let _ = i.send(InputCmd::Event(InputEvent::Sync { leds: leds(flags.contains(CGEventFlags::MaskAlphaShift)) }));
        }
        return false;
    }
    if !s.enabled {
        return false;
    }
    let st = &mut *s;
    let Some((scancode, extended, down)) = decide(&mut st.down, ty, kc, flags.0) else {
        return false;
    };
    let ok = st.input.as_ref().is_some_and(|i| i.send(InputCmd::Event(InputEvent::Key { scancode, extended, down })).is_ok());
    STATS[if ok { 0 } else { 2 }].fetch_add(1, Relaxed);
    true
}

unsafe extern "C-unwind" fn tap_proc(_proxy: CGEventTapProxy, ty: CGEventType, event: NonNull<CGEvent>, _user: *mut c_void) -> *mut CGEvent {
    if ty == CGEventType::TapDisabledByTimeout || ty == CGEventType::TapDisabledByUserInput {
        TAP.with(|t| {
            if let Some(tap) = t.borrow().as_ref() {
                CGEvent::tap_enable(tap, true);
            }
        });
        return event.as_ptr();
    }
    // never let a panic unwind into the window server's callback
    // SAFETY: the event is valid for the duration of the callback.
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(ty, unsafe { event.as_ref() }))) {
        Ok(true) => std::ptr::null_mut(),
        Ok(false) => {
            STATS[1].fetch_add(1, Relaxed);
            event.as_ptr()
        }
        Err(_) => event.as_ptr(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(seq: &[(CGEventType, u16, u64)]) -> Vec<Option<(u16, bool, bool)>> {
        let mut down = BTreeSet::new();
        seq.iter().map(|&(ty, kc, fl)| decide(&mut down, ty, kc, fl)).collect()
    }

    #[test]
    fn combinations_go_to_the_session() {
        use CGEventType as T;
        // Cmd+Tab -> Win+Tab
        assert_eq!(
            run(&[(T::FlagsChanged, 0x37, 0x08), (T::KeyDown, 0x30, 0x08), (T::KeyUp, 0x30, 0x08), (T::FlagsChanged, 0x37, 0)]),
            vec![Some((0x5B, true, true)), Some((0x0F, false, true)), Some((0x0F, false, false)), Some((0x5B, true, false))]
        );
        // right Option pressed while the left one is held, then released
        assert_eq!(
            run(&[(T::FlagsChanged, 0x3A, 0x20), (T::FlagsChanged, 0x3D, 0x60), (T::FlagsChanged, 0x3D, 0x20)]),
            vec![Some((0x38, false, true)), Some((0x38, true, true)), Some((0x38, true, false))]
        );
        // Ctrl+Option+End -> Ctrl+Alt+Del
        let r = run(&[(T::FlagsChanged, 0x3B, 0x01), (T::FlagsChanged, 0x3A, 0x21), (T::KeyDown, 0x77, 0x21)]);
        assert_eq!(r[2], Some((0x53, true, true)));
        // Fn stays local
        assert_eq!(run(&[(T::FlagsChanged, 0x3F, 0)]), vec![None]);
    }

    #[test]
    fn lock_keys() {
        assert_eq!(leds(true), protocol::leds::NUM_LOCK | protocol::leds::CAPS_LOCK);
        assert_eq!(leds(false), protocol::leds::NUM_LOCK);
    }
}
