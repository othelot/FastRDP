//! Windows: while the remote screen is shown in a window, "maximize" (caption button, double click
//! on the title bar, Win+Up, system menu) switches to full screen instead, like mstsc. The
//! `SC_MAXIMIZE` system command is swallowed and reported to the UI.
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use iced::futures::channel::mpsc::Sender;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::UI::Shell::{DefSubclassProc, SetWindowSubclass};
use windows_sys::Win32::UI::WindowsAndMessaging::{SC_MAXIMIZE, WM_SYSCOMMAND};

const SUBCLASS_ID: usize = 0x4652_4D58; // "FRMX"

static ACTIVE: AtomicBool = AtomicBool::new(false);
static SENDER: Mutex<Option<Sender<()>>> = Mutex::new(None);

pub fn install(hwnd: isize) {
    // SAFETY: our top-level window, subclassed from its own (UI) thread.
    unsafe { SetWindowSubclass(hwnd as HWND, Some(subclass_proc), SUBCLASS_ID, 0) };
}

/// Maximize means full screen now (remote screen in a window).
pub fn set_active(on: bool) {
    ACTIVE.store(on, Relaxed);
}

/// Where requests go (the UI subscription).
pub fn set_sender(tx: Sender<()>) {
    *SENDER.lock().unwrap() = Some(tx);
}

unsafe extern "system" fn subclass_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM, _id: usize, _data: usize) -> LRESULT {
    if msg == WM_SYSCOMMAND
        && (wparam & 0xFFF0) as u32 == SC_MAXIMIZE
        && ACTIVE.load(Relaxed)
        && SENDER.lock().unwrap().as_mut().is_some_and(|tx| tx.try_send(()).is_ok())
    {
        return 0;
    }
    // SAFETY: forwarding to the next subclass / the window procedure.
    unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) }
}
