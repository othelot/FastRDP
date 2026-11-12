//! Credentials are checked before a session is created, like RDP's Network Level Authentication:
//! a wrong password is reported to the client instead of starting a session whose logon screen then
//! says "the user name or password is incorrect".
//!
//! `LogonUser` (network logon, nothing is started, the token is closed at once) runs on the
//! connection worker before `OnConnected`. Only definite answers reject the connection; anything
//! else (e.g. the network logon type not being granted by policy) leaves the decision to Windows'
//! own logon, as before. A wrong password counts once towards account lockout, as before (the
//! session logon is not attempted after a rejection).
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows_sys::Win32::Security::{LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, LogonUserW};

use crate::util::wide;

pub enum Verdict {
    Ok,
    /// Win32 error and the message for the user
    Reject(u32, &'static str),
    /// could not be decided here (Win32 error): Windows' logon decides
    Undecided(u32),
}

/// Messages for definite logon failures (Win32 error codes from LogonUser).
pub fn rejection(code: u32) -> Option<&'static str> {
    Some(match code {
        1326 => "The user name or password is incorrect.",
        1327 => "This account cannot sign in this way (for example: accounts with a blank password cannot sign in remotely).",
        1328 => "This account is not allowed to sign in at this time.",
        1329 => "This account is not allowed to sign in to this computer.",
        1330 => "The password of this account has expired. Change it at the computer, then connect again.",
        1331 => "This account is disabled.",
        1793 => "This account has expired.",
        1907 => "The password must be changed before signing in. Change it at the computer, then connect again.",
        1909 => "This account is locked out.",
        _ => return None,
    })
}

pub fn check(domain: &str, user: &str, password: &[u16]) -> Verdict {
    let u = wide(user);
    let d = domain.trim();
    let dw = (!d.is_empty() && d != ".").then(|| wide(d));
    let mut pw: Vec<u16> = password.iter().copied().chain(Some(0)).collect();
    let mut token: HANDLE = std::ptr::null_mut();
    // SAFETY: NUL-terminated strings; the token is closed below.
    let ok = unsafe {
        LogonUserW(u.as_ptr(), dw.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()), pw.as_ptr(), LOGON32_LOGON_NETWORK, LOGON32_PROVIDER_DEFAULT, &mut token)
            != 0
    };
    // SAFETY: trivial.
    let err = if ok { 0 } else { unsafe { GetLastError() } };
    protocol::wipe_u16(&mut pw);
    if ok {
        // SAFETY: token returned by LogonUserW.
        unsafe { CloseHandle(token) };
        return Verdict::Ok;
    }
    match rejection(err) {
        Some(msg) => Verdict::Reject(err, msg),
        None => Verdict::Undecided(err),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_definite_failures_reject() {
        assert!(super::rejection(1326).is_some(), "bad password");
        assert!(super::rejection(1909).is_some(), "locked out");
        assert!(super::rejection(1385).is_none(), "logon type not granted: Windows decides");
        assert!(super::rejection(5).is_none());
    }
}
