//! fastrdp-agent - runs inside an interactive Windows session (started by Task Scheduler at logon /
//! remote connect). Starts Windows' clipboard redirector (rdpclip.exe) in FastRDP sessions, and is
//! the SendInput fallback when the session input devices are unavailable: it connects to the
//! FastRDP server's input pipe and injects keyboard/mouse input with `SendInput` (scancodes,
//! absolute coordinates).
//!
//! Runs as the session user (no extra privileges). Consequences (Windows UIPI / secure desktop):
//! input cannot reach elevated windows unless the agent itself runs elevated, and never reaches the
//! UAC / Ctrl+Alt+Del secure desktop.
#![windows_subsystem = "windows"]

use std::io::Write;
use std::time::Duration;

use protocol::{AgentHello, InputEvent, Message, button, read_message, write_message};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN, XBUTTON1, XBUTTON2};

const PIPE: &str = r"\\.\pipe\FastRDP.Input";

fn log(msg: &str) {
    let Ok(dir) = std::env::var("LOCALAPPDATA") else { return };
    let dir = std::path::Path::new(&dir).join("FastRDP");
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("agent.log")) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
        let _ = writeln!(f, "{t} pid={} {msg}", std::process::id());
    }
}

fn session_id() -> u32 {
    let mut sid = 0u32;
    // SAFETY: valid out pointer.
    unsafe { windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut sid) };
    sid
}

/// One agent per session (the scheduled task fires on logon AND on every remote connect).
fn single_instance() -> bool {
    use windows_sys::Win32::Foundation::{ERROR_ALREADY_EXISTS, GetLastError};
    use windows_sys::Win32::System::Threading::CreateMutexW;
    let name: Vec<u16> = "Local\\FastRDP.Agent".encode_utf16().chain(Some(0)).collect();
    // SAFETY: named mutex in the session namespace; handle intentionally kept for the process lifetime.
    unsafe {
        let h = CreateMutexW(std::ptr::null(), 0, name.as_ptr());
        !h.is_null() && GetLastError() != ERROR_ALREADY_EXISTS
    }
}

/// Follow the input desktop (Default <-> lock screen) so SendInput targets what the user sees.
fn attach_input_desktop() {
    use windows_sys::Win32::System::StationsAndDesktops::{CloseDesktop, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop};
    // SAFETY: desktop handle closed on failure; on success it stays the thread desktop.
    unsafe {
        let d = OpenInputDesktop(0 as DESKTOP_CONTROL_FLAGS, 0, 0x10000000 /* GENERIC_ALL */);
        if !d.is_null() && SetThreadDesktop(d) == 0 {
            CloseDesktop(d);
        }
    }
}

struct Injector {
    keys_down: Vec<(u16, bool)>,
    buttons_down: u8,
}

fn send(inputs: &[INPUT]) -> bool {
    // SAFETY: valid INPUT array.
    let n = unsafe { SendInput(inputs.len() as u32, inputs.as_ptr(), std::mem::size_of::<INPUT>() as i32) };
    n as usize == inputs.len()
}

fn send_retry(inputs: &[INPUT]) {
    if !send(inputs) {
        attach_input_desktop();
        send(inputs);
    }
}

fn key_input(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_KEYBOARD, Anonymous: INPUT_0 { ki: KEYBDINPUT { wVk: vk, wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn mouse_input(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT { r#type: INPUT_MOUSE, Anonymous: INPUT_0 { mi: MOUSEINPUT { dx, dy, mouseData: data as u32, dwFlags: flags, time: 0, dwExtraInfo: 0 } } }
}

fn abs_move(x: u16, y: u16) -> INPUT {
    // Remote desktop pixels of the primary (only) monitor -> 0..65535 normalised coordinates.
    // SAFETY: trivial queries; the process is per-monitor DPI aware, so these are physical pixels.
    let (cx, cy) = unsafe { (GetSystemMetrics(SM_CXSCREEN).max(2), GetSystemMetrics(SM_CYSCREEN).max(2)) };
    let nx = (x as i32).min(cx - 1) * 65535 / (cx - 1);
    let ny = (y as i32).min(cy - 1) * 65535 / (cy - 1);
    mouse_input(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)
}

impl Injector {
    fn key(&mut self, scancode: u16, extended: bool, down: bool) {
        let up = if down { 0 } else { KEYEVENTF_KEYUP };
        let i = match (scancode, extended) {
            // Pause (E1 1D 45), NumLock and the Korean Hangul / Hanja keys: VKs are unambiguous.
            (protocol::SCANCODE_PAUSE, _) => key_input(VK_PAUSE, 0, up),
            (0x45, _) => key_input(VK_NUMLOCK, 0, up | KEYEVENTF_EXTENDEDKEY),
            (0xF2, _) => key_input(VK_HANGUL, 0, up),
            (0xF1, _) => key_input(VK_HANJA, 0, up),
            _ => key_input(0, scancode, KEYEVENTF_SCANCODE | up | if extended { KEYEVENTF_EXTENDEDKEY } else { 0 }),
        };
        send_retry(&[i]);
        if down {
            if !self.keys_down.contains(&(scancode, extended)) {
                self.keys_down.push((scancode, extended));
            }
        } else {
            self.keys_down.retain(|k| *k != (scancode, extended));
        }
    }

    fn button(&mut self, b: u8, down: bool, x: u16, y: u16) {
        let (flags, data) = match (b, down) {
            (button::LEFT, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (button::LEFT, false) => (MOUSEEVENTF_LEFTUP, 0),
            (button::RIGHT, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (button::RIGHT, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (button::MIDDLE, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (button::MIDDLE, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (button::X1, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON1 as i32),
            (button::X2, d) => (if d { MOUSEEVENTF_XDOWN } else { MOUSEEVENTF_XUP }, XBUTTON2 as i32),
            _ => return,
        };
        send_retry(&[abs_move(x, y), mouse_input(0, 0, data, flags)]);
        if down {
            self.buttons_down |= 1 << b;
        } else {
            self.buttons_down &= !(1 << b);
        }
    }

    fn release_all(&mut self) {
        for (sc, ext) in std::mem::take(&mut self.keys_down) {
            self.key(sc, ext, false);
        }
        for b in 0..5u8 {
            if self.buttons_down & (1 << b) != 0 {
                self.button(b, false, 0, 0);
            }
        }
        self.keys_down.clear();
        self.buttons_down = 0;
    }

    fn apply(&mut self, e: InputEvent) {
        match e {
            InputEvent::Key { scancode, extended, down } => self.key(scancode, extended, down),
            InputEvent::Button { button, down, x, y } => self.button(button, down, x, y),
            InputEvent::Wheel { delta, horizontal, x, y } => {
                send_retry(&[abs_move(x, y), mouse_input(0, 0, delta as i32, if horizontal { MOUSEEVENTF_HWHEEL } else { MOUSEEVENTF_WHEEL })])
            }
            InputEvent::MouseMove { x, y } => send_retry(&[abs_move(x, y)]),
            InputEvent::ReleaseAll => self.release_all(),
            InputEvent::Sync { leds } => {
                for (bit, vk) in [(protocol::leds::CAPS_LOCK, VK_CAPITAL), (protocol::leds::NUM_LOCK, VK_NUMLOCK), (protocol::leds::SCROLL_LOCK, VK_SCROLL)] {
                    // SAFETY: trivial query of the toggle bit.
                    let on = unsafe { GetKeyState(vk as i32) } & 1 != 0;
                    if on != (leds & bit != 0) {
                        let ext = if vk == VK_NUMLOCK { KEYEVENTF_EXTENDEDKEY } else { 0 };
                        send_retry(&[key_input(vk, 0, ext), key_input(vk, 0, ext | KEYEVENTF_KEYUP)]);
                    }
                }
            }
        }
    }
}

/// WinStation of this session ("fastrdp#0" when connected through FastRDP).
fn winstation() -> String {
    use windows_sys::Win32::System::RemoteDesktop::{WTS_CURRENT_SESSION, WTSFreeMemory, WTSQuerySessionInformationW, WTSWinStationName};
    // SAFETY: buffer allocated by WTS and freed below.
    unsafe {
        let mut p: *mut u16 = std::ptr::null_mut();
        let mut n = 0u32;
        if WTSQuerySessionInformationW(std::ptr::null_mut(), WTS_CURRENT_SESSION, WTSWinStationName, &mut p, &mut n) == 0 || p.is_null() {
            return String::new();
        }
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        WTSFreeMemory(p as _);
        s
    }
}

fn process_running_in_session(exe: &str, session: u32) -> bool {
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS};
    // SAFETY: snapshot handle closed below; entry struct sized.
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap.is_null() || snap == windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE {
            return false;
        }
        let mut e: PROCESSENTRY32W = std::mem::zeroed();
        e.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
        let mut found = false;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
            if String::from_utf16_lossy(&e.szExeFile[..len]).eq_ignore_ascii_case(exe) {
                let mut sid = u32::MAX;
                windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId(e.th32ProcessID, &mut sid);
                if sid == session {
                    found = true;
                    break;
                }
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        windows_sys::Win32::Foundation::CloseHandle(snap);
        found
    }
}

/// Keeps rdpclip.exe (clipboard redirection, "CLIPRDR" channel) running while the session is
/// connected through FastRDP. RDP starts it from Wds/rdpwd/StartupPrograms, which only applies
/// to its own protocol.
fn rdpclip_keeper(session: u32) {
    let windir = std::env::var("windir").unwrap_or_else(|_| r"C:\Windows".into());
    let exe = std::path::Path::new(&windir).join("System32").join("rdpclip.exe");
    let mut started = 0u32;
    loop {
        if winstation().to_ascii_lowercase().starts_with("fastrdp") && !process_running_in_session("rdpclip.exe", session) {
            match std::process::Command::new(&exe).spawn() {
                Ok(c) => {
                    started += 1;
                    if started <= 20 {
                        log(&format!("started rdpclip.exe pid {}", c.id()));
                    }
                }
                Err(e) => log(&format!("rdpclip.exe: {e}")),
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

fn main() {
    if !single_instance() {
        return;
    }
    // physical pixels for GetSystemMetrics regardless of the session's DPI scaling
    // SAFETY: process-wide setting at startup.
    unsafe {
        windows_sys::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(windows_sys::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let session = session_id();
    log(&format!("agent started in session {session} ({})", winstation()));
    let _ = std::thread::Builder::new().name("rdpclip".into()).spawn(move || rdpclip_keeper(session));
    let mut inj = Injector { keys_down: Vec::new(), buttons_down: 0 };
    loop {
        if let Ok(mut pipe) = std::fs::OpenOptions::new().read(true).write(true).open(PIPE) {
            let hello = Message::AgentHello(AgentHello { session_id: session, pid: std::process::id(), version: 1 });
            if write_message(&mut pipe, &hello).is_ok() {
                log("connected to server");
                attach_input_desktop();
                while let Ok(m) = read_message(&mut pipe) {
                    if let Message::Input(e) = m {
                        inj.apply(e);
                    }
                }
                inj.release_all();
                log("server connection closed");
            }
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}
