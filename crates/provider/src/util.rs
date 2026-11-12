//! Small Win32 helpers (windows-sys) + the panic/HRESULT guard used by every COM entry point.

use std::panic::{AssertUnwindSafe, catch_unwind};

use windows_core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, LocalFree};
use windows_sys::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows_sys::Win32::Security::{GetTokenInformation, LookupAccountSidW, SID_NAME_USE, TOKEN_QUERY, TOKEN_USER, TokenUser};
use windows_sys::Win32::Storage::FileSystem::{GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW};
use windows_sys::Win32::System::LibraryLoader::GetModuleFileNameW;
use windows_sys::Win32::System::Registry::{HKEY, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::{GetProductInfo, GetSystemDirectoryW};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId, OpenProcessToken};

use crate::{loge, logi};

pub const E_NOTIMPL: HRESULT = HRESULT(0x80004001u32 as i32);
pub const E_UNEXPECTED: HRESULT = HRESULT(0x8000FFFFu32 as i32);
pub const E_POINTER: HRESULT = HRESULT(0x80004003u32 as i32);
pub const E_INVALIDARG: HRESULT = HRESULT(0x80070057u32 as i32);
pub const E_FAIL: HRESULT = HRESULT(0x80004005u32 as i32);

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Reads a NUL-terminated UTF-16 string (null pointer -> "").
///
/// # Safety
/// `p` must be null or point to a NUL-terminated UTF-16 string.
pub unsafe fn from_pcwstr(p: *const u16) -> String {
    if p.is_null() {
        return String::new();
    }
    let mut n = 0usize;
    // SAFETY: caller guarantees NUL termination.
    unsafe {
        while *p.add(n) != 0 && n < 32768 {
            n += 1;
        }
        String::from_utf16_lossy(std::slice::from_raw_parts(p, n))
    }
}

/// Copies `src` into a fixed WCHAR array, always NUL-terminating (truncates).
pub fn copy_to_wchar_array(dst: &mut [u16], src: &[u16]) {
    if dst.is_empty() {
        return;
    }
    let n = src.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&src[..n]);
    for x in dst[n..].iter_mut() {
        *x = 0;
    }
}

pub fn guid_str(g: &GUID) -> String {
    format!("{{{:?}}}", g)
}

pub fn hr_of<T>(r: &windows_core::Result<T>) -> u32 {
    match r {
        Ok(_) => 0,
        Err(e) => e.code().0 as u32,
    }
}

/// Runs a COM method body: logs entry/exit HRESULT and converts panics into E_UNEXPECTED.
/// A panic must never unwind across the COM boundary into TermService (it would abort svchost).
pub fn guard<T>(what: &str, f: impl FnOnce() -> windows_core::Result<T>) -> windows_core::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => {
            let hr = hr_of(&r);
            if hr == 0 {
                logi!("{what} -> hr=0x00000000");
            } else {
                // E_NOTIMPL, and ERROR_INSUFFICIENT_BUFFER for size queries, are expected answers
                let expected = hr == E_NOTIMPL.0 as u32 || hr == 0x8007_007A;
                crate::log::write(if expected { "INFO " } else { "WARN " }, &format!("{what} -> hr=0x{hr:08X}"));
            }
            r
        }
        Err(_) => {
            loge!("{what} -> PANIC caught, returning E_UNEXPECTED");
            Err(E_UNEXPECTED.into())
        }
    }
}

/// Like `guard`, for methods TermService polls many times a second (GetProtocolStatus,
/// GetLastInputTime, QueryProperty): only failures other than E_NOTIMPL and panics are logged.
pub fn guard_quiet<T>(what: &str, f: impl FnOnce() -> windows_core::Result<T>) -> windows_core::Result<T> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => {
            let hr = hr_of(&r);
            if hr != 0 && hr != E_NOTIMPL.0 as u32 {
                crate::log::write("WARN ", &format!("{what} -> hr=0x{hr:08X}"));
            }
            r
        }
        Err(_) => {
            loge!("{what} -> PANIC caught, returning E_UNEXPECTED");
            Err(E_UNEXPECTED.into())
        }
    }
}

pub fn reg_dword(root: HKEY, sub: &str, name: &str) -> Option<u32> {
    let (sub, name) = (wide(sub), wide(name));
    let mut v: u32 = 0;
    let mut cb: u32 = 4;
    // SAFETY: valid buffers.
    let rc = unsafe { RegGetValueW(root, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut v as *mut u32).cast(), &mut cb) };
    (rc == 0).then_some(v)
}

pub fn reg_string(root: HKEY, sub: &str, name: &str) -> Option<String> {
    let (sub, name) = (wide(sub), wide(name));
    let mut buf = vec![0u16; 1024];
    let mut cb: u32 = (buf.len() * 2) as u32;
    // SAFETY: valid buffers.
    let rc = unsafe { RegGetValueW(root, sub.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut cb) };
    if rc != 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..n]))
}

pub fn file_version(path: &str) -> String {
    let w = wide(path);
    // SAFETY: Win32 version APIs with correctly sized buffers.
    unsafe {
        let mut dummy = 0u32;
        let sz = GetFileVersionInfoSizeW(w.as_ptr(), &mut dummy);
        if sz == 0 {
            return "?".into();
        }
        let mut data = vec![0u8; sz as usize];
        if GetFileVersionInfoW(w.as_ptr(), 0, sz, data.as_mut_ptr().cast()) == 0 {
            return "?".into();
        }
        let mut p: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut len = 0u32;
        let root = wide("\\");
        if VerQueryValueW(data.as_ptr().cast(), root.as_ptr(), &mut p, &mut len) == 0 || p.is_null() {
            return "?".into();
        }
        let ffi = &*(p as *const VS_FIXEDFILEINFO);
        let fixed = format!("{}.{}.{}.{}", ffi.dwFileVersionMS >> 16, ffi.dwFileVersionMS & 0xFFFF, ffi.dwFileVersionLS >> 16, ffi.dwFileVersionLS & 0xFFFF);
        // String resource (what Explorer/PowerShell show) via the first translation entry.
        let mut tp: *mut core::ffi::c_void = std::ptr::null_mut();
        let mut tlen = 0u32;
        let tq = wide(r"\VarFileInfo\Translation");
        let mut string_ver = String::from("?");
        if VerQueryValueW(data.as_ptr().cast(), tq.as_ptr(), &mut tp, &mut tlen) != 0 && tlen >= 4 {
            let lang = *(tp as *const u16);
            let cp = *((tp as *const u16).add(1));
            let q = wide(&format!(r"\StringFileInfo\{lang:04x}{cp:04x}\FileVersion"));
            let mut sp: *mut core::ffi::c_void = std::ptr::null_mut();
            let mut slen = 0u32;
            if VerQueryValueW(data.as_ptr().cast(), q.as_ptr(), &mut sp, &mut slen) != 0 && !sp.is_null() {
                string_ver = from_pcwstr(sp as *const u16);
            }
        }
        format!("{string_ver} (fixed {fixed})")
    }
}

pub fn current_session_id() -> u32 {
    let mut sid = u32::MAX;
    // SAFETY: valid out pointer.
    unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) };
    sid
}

fn token_user() -> String {
    // SAFETY: standard token query sequence, buffers sized by the API.
    unsafe {
        let mut tok: HANDLE = std::ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut tok) == 0 {
            return "?".into();
        }
        let mut buf = [0u64; 64]; // 8-byte aligned for TOKEN_USER
        let mut cb = 0u32;
        let mut out = "?".to_string();
        if GetTokenInformation(tok, TokenUser, buf.as_mut_ptr().cast(), (buf.len() * 8) as u32, &mut cb) != 0 {
            let tu = &*(buf.as_ptr() as *const TOKEN_USER);
            let mut s: *mut u16 = std::ptr::null_mut();
            if ConvertSidToStringSidW(tu.User.Sid, &mut s) != 0 {
                out = from_pcwstr(s);
                LocalFree(s.cast());
            }
            let mut name = [0u16; 128];
            let mut dom = [0u16; 128];
            let (mut cn, mut cd) = (128u32, 128u32);
            let mut use_: SID_NAME_USE = 0;
            if LookupAccountSidW(std::ptr::null(), tu.User.Sid, name.as_mut_ptr(), &mut cn, dom.as_mut_ptr(), &mut cd, &mut use_) != 0 {
                out = format!("{out} ({}\\{})", from_pcwstr(dom.as_ptr()), from_pcwstr(name.as_ptr()));
            }
        }
        CloseHandle(tok);
        out
    }
}

pub fn module_path(module: *mut core::ffi::c_void) -> String {
    let mut buf = [0u16; 520];
    // SAFETY: valid buffer; module may be null (= exe).
    let n = unsafe { GetModuleFileNameW(module, buf.as_mut_ptr(), buf.len() as u32) } as usize;
    String::from_utf16_lossy(&buf[..n])
}

pub fn os_summary() -> String {
    const CV: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    use windows_sys::Win32::System::Registry::HKEY_LOCAL_MACHINE as HKLM;
    let build = reg_string(HKLM, CV, "CurrentBuildNumber").unwrap_or_else(|| "?".into());
    let ubr = reg_dword(HKLM, CV, "UBR").map(|v| v.to_string()).unwrap_or_else(|| "?".into());
    let edition = reg_string(HKLM, CV, "EditionID").unwrap_or_else(|| "?".into());
    let dv = reg_string(HKLM, CV, "DisplayVersion").unwrap_or_else(|| "?".into());
    let mut pt = 0u32;
    // SAFETY: valid out pointer.
    unsafe { GetProductInfo(10, 0, 0, 0, &mut pt) };
    format!("Windows 10.0.{build}.{ubr} Edition={edition} DisplayVersion={dv} ProductType=0x{pt:X}")
}

pub fn termsrv_version() -> String {
    let mut buf = [0u16; 260];
    // SAFETY: valid buffer.
    let n = unsafe { GetSystemDirectoryW(buf.as_mut_ptr(), buf.len() as u32) } as usize;
    let dir = String::from_utf16_lossy(&buf[..n]);
    file_version(&format!("{dir}\\termsrv.dll"))
}

pub fn log_environment(reason: &str) {
    logi!("==== environment ({reason}) ====");
    logi!("OS: {}", os_summary());
    logi!(
        "Process: pid={} sessionId={} image={} user={}",
        // SAFETY: trivial.
        unsafe { GetCurrentProcessId() },
        current_session_id(),
        module_path(std::ptr::null_mut()),
        token_user()
    );
    logi!(
        "Module: {} ; termsrv.dll={} ; provider={} ; log={}",
        module_path(crate::module_handle()),
        termsrv_version(),
        env!("CARGO_PKG_VERSION"),
        crate::log::log_path()
    );
}
