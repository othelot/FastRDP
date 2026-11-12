//! File + OutputDebugString logger for code running inside TermService's svchost.
//!
//! Path order:
//!  1. `%ProgramData%\FastRDP\logs\provider.log` - created by the installer with write access for
//!     NETWORK SERVICE (TermService account). Not created here, so ACLs stay under installer control.
//!  2. `%TEMP%\FastRDP\provider.log`
//!
//! Never pass passwords to these functions; `protocol::Secret` has a redacting `Debug`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::SYSTEMTIME;
use windows_sys::Win32::System::Diagnostics::Debug::OutputDebugStringW;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{GetCurrentProcessId, GetCurrentThreadId};

static LOG: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();

fn writable(p: &PathBuf) -> bool {
    OpenOptions::new().create(true).append(true).open(p).is_ok()
}

fn choose_path() -> Option<PathBuf> {
    if let Ok(pd) = std::env::var("ProgramData") {
        let dir = PathBuf::from(pd).join("FastRDP").join("logs");
        if dir.is_dir() {
            let p = dir.join("provider.log");
            if writable(&p) {
                return Some(p);
            }
        }
    }
    let dir = std::env::temp_dir().join("FastRDP");
    let _ = std::fs::create_dir_all(&dir);
    let p = dir.join("provider.log");
    writable(&p).then_some(p)
}

fn state() -> &'static Mutex<Option<PathBuf>> {
    LOG.get_or_init(|| {
        let p = choose_path();
        if let Some(p) = &p
            && let Ok(md) = std::fs::metadata(p)
            && md.len() > 16 * 1024 * 1024
        {
            let _ = std::fs::rename(p, p.with_extension("log.1"));
        }
        Mutex::new(p)
    })
}

pub fn log_path() -> String {
    state().lock().map(|g| g.as_ref().map(|p| p.display().to_string()).unwrap_or_default()).unwrap_or_default()
}

pub fn write(level: &str, msg: &str) {
    let mut st = SYSTEMTIME { wYear: 0, wMonth: 0, wDayOfWeek: 0, wDay: 0, wHour: 0, wMinute: 0, wSecond: 0, wMilliseconds: 0 };
    // SAFETY: plain Win32 calls with valid out pointers.
    let (pid, tid) = unsafe {
        GetLocalTime(&mut st);
        (GetCurrentProcessId(), GetCurrentThreadId())
    };
    let line = format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} pid={} tid={} {} {}\r\n",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds, pid, tid, level, msg
    );
    let dbg: Vec<u16> = format!("[FastRDP] {line}").encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: NUL-terminated UTF-16 buffer.
    unsafe { OutputDebugStringW(dbg.as_ptr()) };

    if let Ok(guard) = state().lock()
        && let Some(p) = guard.as_ref()
        && let Ok(mut f) = OpenOptions::new().create(true).append(true).open(p)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! logi { ($($t:tt)*) => { $crate::log::write("INFO ", &format!($($t)*)) } }
#[macro_export]
macro_rules! logw { ($($t:tt)*) => { $crate::log::write("WARN ", &format!($($t)*)) } }
#[macro_export]
macro_rules! loge { ($($t:tt)*) => { $crate::log::write("ERROR", &format!($($t)*)) } }
