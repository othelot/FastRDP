//! Session arbitration: reconnecting to the user's existing session, like RDP.
//!
//! Windows' default arbitration only offers sessions whose type matches the connection's terminal
//! type (termsrv `CUtils::IsTerminalCompatibleToSession`). The terminal type is "RDP" only for the
//! two protocol CLSIDs termsrv recognises (`IsRDPProtocol`: rdpcorets and a test protocol); every
//! other protocol is a different type. So a FastRDP connection never reconnected to the user's
//! RDP session (a new session was logged on next to it), and RDP never reconnected to FastRDP's
//! sessions; only the console is exempt.
//!
//! The documented way for a protocol to choose the session is
//! `IWRdsProtocolConnection::SessionArbitrationEnumeration`: when it returns sessions, termsrv
//! skips its default filter and reconnects to them. termsrv calls it twice (termsrv
//! `CSessionArbitrationDesktop::EnumerateSessionsUsingProtocolExtension`): first without an array
//! (answer: the count and `HRESULT_FROM_WIN32(ERROR_INSUFFICIENT_BUFFER)`), then with an array of
//! that size. (An earlier version answered the first call with S_OK, termsrv then used an
//! unallocated array and terminated.)
//!
//! Sessions created by RDP cannot be taken over at all: lsm.dll checks the same compatibility again
//! when it applies the connection to the session (`CSessionReconnectController::
//! TryApplyTerminalToSession` -> ERROR_CTX_SESSION_INCOMPATIBLE, "the target session is
//! incompatible with the current session"). They are therefore never named here (client protocol
//! type RDP); for them Windows' default arbitration applies.
//!
//! The user's sessions are listed when the connection is accepted, on the provider's own thread
//! (no RDS API calls inside TermService callbacks). The answer is only given when the token of the
//! user who actually logged on has the same SID; otherwise Windows' default arbitration applies.
use windows_sys::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, GetLastError, HANDLE};
use windows_sys::Win32::Security::{GetLengthSid, GetTokenInformation, LookupAccountNameW, SID_NAME_USE, TOKEN_USER, TokenUser};
use windows_sys::Win32::System::RemoteDesktop::{
    WTS_INFO_CLASS, WTS_SESSION_INFOW, WTSActive, WTSClientProtocolType, WTSDisconnected, WTSDomainName, WTSEnumerateSessionsW, WTSFreeMemory,
    WTSQuerySessionInformationW, WTSUserName,
};

use crate::util::wide;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UserSession {
    pub id: u32,
    pub active: bool,
    /// created by Microsoft's RDP (client protocol type RDP): Windows does not let another
    /// protocol reconnect to it
    pub rdp: bool,
}

/// The connecting account and its sessions when the connection was accepted.
#[derive(Clone, Debug)]
pub struct Existing {
    pub sid: Vec<u8>,
    pub sessions: Vec<UserSession>,
}

fn lookup_sid(name: &str) -> Option<Vec<u8>> {
    let n = wide(name);
    let (mut cb_sid, mut cch_dom) = (0u32, 0u32);
    let mut use_ = SID_NAME_USE::default();
    // SAFETY: size query with null buffers, then sized buffers.
    unsafe {
        LookupAccountNameW(std::ptr::null(), n.as_ptr(), std::ptr::null_mut(), &mut cb_sid, std::ptr::null_mut(), &mut cch_dom, &mut use_);
        if GetLastError() != ERROR_INSUFFICIENT_BUFFER || cb_sid == 0 {
            return None;
        }
        let mut sid = vec![0u8; cb_sid as usize];
        let mut dom = vec![0u16; cch_dom as usize + 1];
        if LookupAccountNameW(std::ptr::null(), n.as_ptr(), sid.as_mut_ptr().cast(), &mut cb_sid, dom.as_mut_ptr(), &mut cch_dom, &mut use_) == 0 {
            return None;
        }
        sid.truncate(cb_sid as usize);
        Some(sid)
    }
}

/// SID of the account as entered by the client (`user`, `.\user`, `DOMAIN\user`, `user@domain`).
pub fn account_sid(domain: &str, user: &str) -> Option<Vec<u8>> {
    let d = domain.trim();
    let mut names = Vec::new();
    if d.is_empty() || d == "." {
        names.push(user.to_string());
    } else {
        names.push(format!("{d}\\{user}"));
        if d.contains('.') {
            names.push(format!("{user}@{d}"));
        }
    }
    names.iter().find_map(|n| lookup_sid(n))
}

fn session_string(id: u32, class: WTS_INFO_CLASS) -> String {
    let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut bytes = 0u32;
    // SAFETY: WTS allocates the buffer; freed with WTSFreeMemory.
    unsafe {
        if WTSQuerySessionInformationW(std::ptr::null_mut(), id, class, &mut buf, &mut bytes) == 0 || buf.is_null() {
            return String::new();
        }
        let s = crate::util::from_pcwstr(buf);
        WTSFreeMemory(buf.cast());
        s
    }
}

/// WTS_PROTOCOL_TYPE of the session's (last) client: 0 console (also FastRDP), 2 RDP.
fn client_protocol_type(id: u32) -> Option<u16> {
    let mut buf: windows_sys::core::PWSTR = std::ptr::null_mut();
    let mut bytes = 0u32;
    // SAFETY: WTS allocates the buffer (a USHORT); freed with WTSFreeMemory.
    unsafe {
        if WTSQuerySessionInformationW(std::ptr::null_mut(), id, WTSClientProtocolType, &mut buf, &mut bytes) == 0 || buf.is_null() {
            return None;
        }
        let v = (bytes >= 2).then(|| *(buf as *const u16));
        WTSFreeMemory(buf.cast());
        v
    }
}

/// Active and disconnected sessions whose user is `sid`.
pub fn sessions_of(sid: &[u8]) -> Vec<UserSession> {
    let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
    let mut count = 0u32;
    // SAFETY: WTS_CURRENT_SERVER_HANDLE (null); array freed with WTSFreeMemory.
    let list: Vec<(u32, i32)> = unsafe {
        if WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut info, &mut count) == 0 {
            return Vec::new();
        }
        let v = std::slice::from_raw_parts(info, count as usize).iter().map(|s| (s.SessionId, s.State)).collect();
        WTSFreeMemory(info.cast());
        v
    };
    list.into_iter()
        .filter(|(_, state)| *state == WTSActive || *state == WTSDisconnected)
        .filter(|(id, _)| {
            let user = session_string(*id, WTSUserName);
            !user.is_empty() && account_sid(&session_string(*id, WTSDomainName), &user).as_deref() == Some(sid)
        })
        .map(|(id, state)| UserSession { id, active: state == WTSActive, rdp: client_protocol_type(id) == Some(2) })
        .collect()
}

pub fn lookup(domain: &str, user: &str) -> Option<Existing> {
    let sid = account_sid(domain, user)?;
    Some(Existing { sessions: sessions_of(&sid), sid })
}

/// User SID of a token (TermService's handle: only read, never closed here).
pub fn token_sid(token: HANDLE) -> Option<Vec<u8>> {
    let mut len = 0u32;
    // SAFETY: size query, then a sized, 8-byte aligned buffer; the SID is copied out by its length.
    unsafe {
        GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut len);
        if len == 0 {
            return None;
        }
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        if GetTokenInformation(token, TokenUser, buf.as_mut_ptr().cast(), len, &mut len) == 0 {
            return None;
        }
        let tu = &*(buf.as_ptr() as *const TOKEN_USER);
        let n = GetLengthSid(tu.User.Sid) as usize;
        Some(std::slice::from_raw_parts(tu.User.Sid as *const u8, n).to_vec())
    }
}

/// The session to reconnect to: the user's active session (in use by another connection, which
/// Windows then disconnects - like RDP), else the oldest disconnected one; never the session that
/// was just created for this connection.
/// `rdp_compatible`: the listener uses the CLSID termsrv treats as RDP, so sessions created by RDP
/// are compatible too.
pub fn pick(sessions: &[UserSession], new_session: u32, rdp_compatible: bool) -> Option<u32> {
    let mut c: Vec<&UserSession> = sessions.iter().filter(|s| s.id != new_session && (rdp_compatible || !s.rdp)).collect();
    c.sort_by_key(|s| (!s.active, s.id));
    c.first().map(|s| s.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_first_then_oldest_disconnected() {
        let s = |id, active| UserSession { id, active, rdp: false };
        assert_eq!(pick(&[s(3, false), s(2, true)], 6, false), Some(2));
        assert_eq!(pick(&[s(5, false), s(3, false)], 6, false), Some(3));
        assert_eq!(pick(&[s(6, true)], 6, true), None, "never the new session itself");
        assert_eq!(pick(&[], 6, true), None);
        let rdp = UserSession { id: 2, active: true, rdp: true };
        assert_eq!(pick(&[rdp, s(3, false)], 6, false), Some(3), "own CLSID: never a session made by RDP");
        assert_eq!(pick(&[rdp], 6, false), None);
        assert_eq!(pick(&[rdp, s(3, false)], 6, true), Some(2), "RDP-compatible: the active RDP session");
    }
}
