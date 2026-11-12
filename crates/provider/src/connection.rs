//! One FastRDP client connection as seen by TermService:
//! `IWRdsProtocolConnection` + `IWRdsWddmIddProps` + `IWRdsWddmIddProps1`, plus the
//! license connection and logon-error redirector objects it hands out.
//!
//! Credential handling: the password lives only in a [`Secret`] (zeroed on drop) and is copied
//! into TermService's `WRDS_USER_CREDENTIAL` buffer in `GetUserCredentials`. It is wiped as soon
//! as `LogonNotify`, `Close` or a failed `OnConnected` happens. It is never logged.

use crate::transport::Transport;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use protocol::{Event, EventMsg, Message, Secret, write_message};
use windows::Win32::Foundation::HANDLE_PTR;
use windows::Win32::System::RemoteDesktop::*;
use windows_core::{BOOL, GUID, PCSTR, PCWSTR, implement};
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

use crate::config::ListenerConfig;
use crate::util::{E_NOTIMPL, E_POINTER, copy_to_wchar_array, from_pcwstr, guard, guid_str};
use crate::{logi, logw};

static NEXT_ID: AtomicU32 = AtomicU32::new(1);

/// TermService objects are free-threaded; we only ever call them from MTA threads.
pub struct Agile<T>(pub T);
// SAFETY: the wrapped COM pointers belong to TermService's MTA objects (or our own
// free-threaded objects) and are only used from MTA-initialised threads.
unsafe impl<T> Send for Agile<T> {}
// SAFETY: see above.
unsafe impl<T> Sync for Agile<T> {}

#[derive(Clone, Debug)]
pub struct ClientInfo {
    pub width: u16,
    pub height: u16,
    pub refresh_hz: u16,
    pub client_name: String,
    pub peer_ip: String,
    pub peer_is_v6: bool,
    pub performance_flags: u32,
}

pub struct Credentials {
    pub domain: String,
    pub user: String,
    pub password: Secret,
}

impl Credentials {
    pub fn wipe(&mut self) {
        self.password.wipe();
    }
}

struct State {
    creds: Option<Credentials>,
    callback: Option<Agile<IWRdsProtocolConnectionCallback>>,
    driver_handle: HANDLE_PTR,
    session_id: u32,
    wddm_idd: bool,
    /// session keyboard/mouse (terminpt.sys), see terminput.rs
    input: Option<Arc<crate::terminput::SessionInput>>,
    channels: Vec<Arc<crate::channels::Channel>>,
}

#[implement(IWRdsProtocolConnection, IWRdsWddmIddProps, IWRdsWddmIddProps1)]
pub struct Connection {
    pub id: u32,
    cfg: ListenerConfig,
    client: ClientInfo,
    created: Instant,
    state: Mutex<State>,
    closed: AtomicBool,
    broken_sent: AtomicBool,
    /// transport to fastrdp-server, shared with the virtual-channel reader threads
    writer: Arc<Mutex<Option<Box<dyn Transport>>>>,
    next_channel: AtomicU32,
}

impl Connection {
    pub fn new(cfg: ListenerConfig, client: ClientInfo, writer: Box<dyn Transport>, creds: Credentials) -> Self {
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        logi!(
            "Connection#{id}: created for peer={} client='{}' user='{}\\{}' {}x{}@{}",
            client.peer_ip,
            client.client_name,
            creds.domain,
            creds.user,
            client.width,
            client.height,
            client.refresh_hz
        );
        Connection {
            id,
            cfg,
            client,
            created: Instant::now(),
            state: Mutex::new(State {
                creds: Some(creds),
                callback: None,
                driver_handle: HANDLE_PTR(0),
                session_id: u32::MAX,
                wddm_idd: false,
                input: None,
                channels: Vec::new(),
            }),
            closed: AtomicBool::new(false),
            broken_sent: AtomicBool::new(false),
            writer: Arc::new(Mutex::new(Some(writer))),
            next_channel: AtomicU32::new(1),
        }
    }

    fn tag(&self) -> String {
        format!("Connection#{}", self.id)
    }

    /// Sends a status event to the client (best effort, never blocks TermService long:
    /// the socket has a 2 s write timeout).
    pub fn event(&self, ev: Event, value: u32, text: &str) {
        let m = Message::Event(EventMsg { event: ev as u32, value, text: text.to_string() });
        if !send_on(&self.writer, &m) {
            logw!("{}: event {:?} not delivered", self.tag(), ev);
        }
    }

    /// Sender for the virtual-channel reader threads.
    fn sender(&self) -> crate::channels::Sender {
        let w = self.writer.clone();
        Arc::new(move |m: &Message| send_on(&w, m))
    }

    /// Client -> session component (one channel message).
    pub fn channel_write(&self, id: u32, data: &[u8]) {
        let ch = self.state.lock().ok().and_then(|s| s.channels.iter().find(|c| c.id == id).cloned());
        match ch {
            Some(c) => {
                c.write(data);
            }
            None => logw!("{}: data for unknown channel #{id} dropped", self.tag()),
        }
    }

    /// Input from the server (coordinates normalized 0..=65535) -> session devices.
    pub fn inject(&self, e: &protocol::InputEvent) {
        let input = self.state.lock().ok().and_then(|s| s.input.clone());
        if let Some(i) = input {
            i.inject(e);
        }
    }

    fn drop_input(&self, why: &str) {
        let old = self.state.lock().ok().and_then(|mut s| s.input.take());
        if let Some(i) = old {
            logi!("{}: removing input devices of session {} ({why})", self.tag(), i.session);
            drop(i);
        }
    }

    fn wipe_credentials(&self, why: &str) {
        if let Ok(mut st) = self.state.lock()
            && let Some(mut c) = st.creds.take()
        {
            c.wipe();
            logi!("{}: credentials wiped ({why})", self.tag());
        }
    }

    /// Takes ownership of the callback reference returned by OnConnected.
    pub fn attach_callback(&self, cb: IWRdsProtocolConnectionCallback) {
        self.state.lock().unwrap().callback = Some(Agile(cb));
    }

    pub fn callback(&self) -> Option<IWRdsProtocolConnectionCallback> {
        self.state.lock().ok()?.callback.as_ref().map(|a| a.0.clone())
    }

    /// OnConnected failed: drop everything immediately.
    pub fn abort_before_accept(&self) {
        self.wipe_credentials("OnConnected failed");
        self.closed.store(true, Ordering::SeqCst);
    }

    pub fn closed_by_service(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Transport ended (client BYE / EOF / error). Tell TermService unless it already closed us.
    pub fn on_transport_closed(&self, why: &str) {
        logi!("{}: transport closed ({why})", self.tag());
        if self.closed.load(Ordering::SeqCst) || self.broken_sent.swap(true, Ordering::SeqCst) {
            return;
        }
        if let Some(cb) = self.callback() {
            // SAFETY: valid TermService callback; called outside our locks.
            let r = unsafe { cb.BrokenConnection(0, 0) };
            logi!("{}: IWRdsProtocolConnectionCallback::BrokenConnection -> hr=0x{:08X}", self.tag(), crate::util::hr_of(&r));
        }
    }

    /// Worker is about to drop the socket: stop sending.
    pub fn detach_socket(&self) {
        if let Ok(mut w) = self.writer.lock()
            && let Some(w) = w.take()
        {
            w.shutdown();
        }
    }

    fn release_driver_handle(&self, st: &mut State) {
        if st.driver_handle.0 != 0 {
            // SAFETY: TermService transferred ownership of this handle to us in OnDriverLoad.
            unsafe { CloseHandle(st.driver_handle.0 as HANDLE) };
            st.driver_handle = HANDLE_PTR(0);
        }
    }

    fn hardware_id(&self, buf: &PCWSTR, count: u32, which: &str) -> windows_core::Result<()> {
        if buf.0.is_null() || count == 0 {
            return Err(E_POINTER.into());
        }
        let id: Vec<u16> = self.cfg.idd_hardware_id.encode_utf16().collect();
        // SAFETY: TermService passes a writable buffer of `count` WCHARs (the IDL marks it [in]
        // but the contract is that the protocol fills it, as in Microsoft's sample).
        let dst = unsafe { std::slice::from_raw_parts_mut(buf.0 as *mut u16, count as usize) };
        copy_to_wchar_array(dst, &id);
        logi!("{}: {which}::GetHardwareId count={count} -> '{}'", self.tag(), self.cfg.idd_hardware_id);
        self.event(Event::HardwareIdRequested, 0, &self.cfg.idd_hardware_id);
        Ok(())
    }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(st) = self.state.get_mut() {
            if let Some(mut c) = st.creds.take() {
                c.wipe();
            }
            if st.driver_handle.0 != 0 {
                // SAFETY: owned handle (see OnDriverLoad).
                unsafe { CloseHandle(st.driver_handle.0 as HANDLE) };
            }
        }
        logi!("Connection#{}: destroyed after {:?}", self.id, self.created.elapsed());
    }
}

impl IWRdsProtocolConnection_Impl for Connection_Impl {
    fn GetLogonErrorRedirector(&self) -> windows_core::Result<IWRdsProtocolLogonErrorRedirector> {
        guard(&format!("{}::GetLogonErrorRedirector", self.tag()), || {
            // We implement it purely to *log* winlogon's status / error messages, which is the best
            // available evidence when a provider logon fails. Every message stays NOT_HANDLED.
            Ok(LogonErrorRedirector { conn_id: self.id }.into())
        })
    }

    fn AcceptConnection(&self) -> windows_core::Result<()> {
        guard(&format!("{}::AcceptConnection", self.tag()), || Ok(()))
    }

    fn GetClientData(&self, p: *mut WTS_CLIENT_DATA) -> windows_core::Result<()> {
        guard(&format!("{}::GetClientData", self.tag()), || {
            if p.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: TermService passes a WRDS_CLIENT_DATA it owns; we only write fields.
            let d = unsafe { &mut *p };
            d.fEnableWindowsKey = true;
            d.fInheritAutoLogon = BOOL(1); // use credentials supplied by GetUserCredentials
            d.fPromptForPassword = false;
            d.fNoAudioPlayback = !self.cfg.virtual_channels;
            d.fMouse = true;
            // session-only experience settings (wallpaper, animations, themes, ClearType ...)
            d.PerformanceFlags = self.client.performance_flags;
            d.HRes = self.client.width;
            d.VRes = self.client.height;
            copy_to_wchar_array(&mut d.ProtocolName, &crate::PROTOCOL_NAME.encode_utf16().collect::<Vec<_>>());
            copy_to_wchar_array(&mut d.ClientName, &self.client.client_name.encode_utf16().collect::<Vec<_>>());
            copy_to_wchar_array(&mut d.ClientAddress, &self.client.peer_ip.encode_utf16().collect::<Vec<_>>());
            d.ClientAddressFamily = if self.client.peer_is_v6 { 23 } else { 2 };
            d.ClientBuildNumber = 1;
            logi!(
                "{}::GetClientData filled PerformanceFlags=0x{:X} ProtocolName={} HRes={} VRes={} ClientName='{}' ClientAddress={} (incoming fInheritAutoLogon was overwritten)",
                self.tag(),
                d.PerformanceFlags,
                crate::PROTOCOL_NAME,
                d.HRes,
                d.VRes,
                self.client.client_name,
                self.client.peer_ip
            );
            Ok(())
        })
    }

    fn GetClientMonitorData(&self, pnum: *mut u32, pprimary: *mut u32) -> windows_core::Result<()> {
        guard(&format!("{}::GetClientMonitorData", self.tag()), || {
            // SAFETY: out pointers from TermService (checked).
            unsafe {
                if !pnum.is_null() {
                    *pnum = 1;
                }
                if !pprimary.is_null() {
                    *pprimary = 0;
                }
            }
            Ok(())
        })
    }

    fn GetUserCredentials(&self, p: *mut WTS_USER_CREDENTIAL) -> windows_core::Result<()> {
        guard(&format!("{}::GetUserCredentials", self.tag()), || {
            if p.is_null() {
                return Err(E_POINTER.into());
            }
            let st = self.state.lock().unwrap();
            let Some(c) = st.creds.as_ref() else {
                logw!("{}::GetUserCredentials: no credentials held (already wiped or none supplied)", self.tag());
                return Err(E_NOTIMPL.into());
            };
            // SAFETY: TermService-owned output struct.
            let out = unsafe { &mut *p };
            copy_to_wchar_array(&mut out.UserName, &c.user.encode_utf16().collect::<Vec<_>>());
            copy_to_wchar_array(&mut out.Domain, &c.domain.encode_utf16().collect::<Vec<_>>());
            copy_to_wchar_array(&mut out.Password, c.password.as_utf16());
            logi!(
                "{}::GetUserCredentials supplied user='{}\\{}' password=<redacted, {}>",
                self.tag(),
                c.domain,
                c.user,
                if c.password.is_empty() { "empty" } else { "present" }
            );
            Ok(())
        })
    }

    fn GetLicenseConnection(&self) -> windows_core::Result<IWRdsProtocolLicenseConnection> {
        guard(&format!("{}::GetLicenseConnection", self.tag()), || {
            Ok(LicenseConnection { conn_id: self.id, client_name: self.client.client_name.clone() }.into())
        })
    }

    fn AuthenticateClientToSession(&self, _sessionid: *mut WTS_SESSION_ID) -> windows_core::Result<()> {
        // No reconnect support: E_NOTIMPL -> "the service continues the connection sequence".
        guard(&format!("{}::AuthenticateClientToSession", self.tag()), || Err(E_NOTIMPL.into()))
    }

    fn NotifySessionId(&self, sessionid: *const WTS_SESSION_ID, sessionhandle: HANDLE_PTR) -> windows_core::Result<()> {
        guard(&format!("{}::NotifySessionId", self.tag()), || {
            if sessionid.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: valid pointer from TermService.
            let s = unsafe { &*sessionid };
            logi!(
                "{}::NotifySessionId sessionId={} uniqueGuid={} sessionHandle=0x{:X}",
                self.tag(),
                s.SessionId,
                guid_str(&s.SessionUniqueGuid),
                sessionhandle.0
            );
            self.state.lock().unwrap().session_id = s.SessionId;
            self.event(Event::SessionId, s.SessionId, "");
            Ok(())
        })
    }

    fn GetInputHandles(&self, pk: *mut HANDLE_PTR, pm: *mut HANDLE_PTR, pb: *mut HANDLE_PTR) -> windows_core::Result<()> {
        guard(&format!("{}::GetInputHandles", self.tag()), || {
            // SAFETY: out pointers from TermService (checked). Beep stays NULL.
            unsafe {
                for p in [pk, pm, pb] {
                    if !p.is_null() {
                        *p = HANDLE_PTR(0);
                    }
                }
            }
            let session = self.state.lock().map(|s| s.session_id).unwrap_or(u32::MAX);
            if !self.cfg.device_input || session == u32::MAX {
                logi!(
                    "{}::GetInputHandles -> NULL handles (device input {})",
                    self.tag(),
                    if self.cfg.device_input { "enabled, no session id yet" } else { "disabled" }
                );
                self.event(Event::InputDevices, 0, "device input disabled");
                return Ok(());
            }
            // Called again after a reconnect to an existing session: devices belong to one session.
            self.drop_input("new session id");
            match crate::terminput::SessionInput::create(session, &format!("C{}", self.id)) {
                Ok(input) => {
                    let (k, m) = input.handles;
                    // SAFETY: as above.
                    unsafe {
                        if !pk.is_null() {
                            *pk = HANDLE_PTR(k as usize);
                        }
                        if !pm.is_null() {
                            *pm = HANDLE_PTR(m as usize);
                        }
                    }
                    logi!("{}::GetInputHandles -> session {session} keyboard={k:?} mouse={m:?} (terminpt)", self.tag());
                    if let Ok(mut s) = self.state.lock() {
                        s.input = Some(Arc::new(input));
                    }
                    self.event(Event::InputDevices, 1, &session.to_string());
                }
                Err(e) => {
                    logw!("{}::GetInputHandles: session devices unavailable ({e}) -> NULL handles", self.tag());
                    self.event(Event::InputDevices, 0, &e);
                }
            }
            Ok(())
        })
    }

    fn GetVideoHandle(&self) -> windows_core::Result<HANDLE_PTR> {
        guard(&format!("{}::GetVideoHandle", self.tag()), || {
            logw!("{}::GetVideoHandle called (legacy XDDM path?) -> NULL", self.tag());
            Ok(HANDLE_PTR(0))
        })
    }

    fn ConnectNotify(&self, sessionid: u32) -> windows_core::Result<()> {
        guard(&format!("{}::ConnectNotify", self.tag()), || {
            logi!("{}::ConnectNotify sessionId={sessionid}", self.tag());
            self.event(Event::ConnectNotify, sessionid, "");
            Ok(())
        })
    }

    fn IsUserAllowedToLogon(&self, sessionid: u32, _token: HANDLE_PTR, dom: &PCWSTR, user: &PCWSTR) -> windows_core::Result<()> {
        guard(&format!("{}::IsUserAllowedToLogon", self.tag()), || {
            // SAFETY: NUL-terminated strings from TermService. The token is NOT ours to close.
            let (d, u) = unsafe { (from_pcwstr(dom.0), from_pcwstr(user.0)) };
            logi!("{}::IsUserAllowedToLogon sessionId={sessionid} user='{d}\\{u}' -> allowed", self.tag());
            Ok(())
        })
    }

    fn SessionArbitrationEnumeration(&self, _tok: HANDLE_PTR, single: BOOL, _arr: *mut u32, pcount: *mut u32) -> windows_core::Result<()> {
        guard(&format!("{}::SessionArbitrationEnumeration", self.tag()), || {
            // SAFETY: in/out count pointer from TermService.
            let n = if pcount.is_null() { 0 } else { unsafe { *pcount } };
            logi!("{}::SessionArbitrationEnumeration bSingleSessionPerUserEnabled={} capacity={n} -> E_NOTIMPL (default arbitration)", self.tag(), single.0);
            Err(E_NOTIMPL.into())
        })
    }

    fn LogonNotify(
        &self,
        _tok: HANDLE_PTR,
        user: &PCWSTR,
        dom: &PCWSTR,
        sessionid: *const WTS_SESSION_ID,
        _settings: *mut WRDS_CONNECTION_SETTINGS,
    ) -> windows_core::Result<()> {
        guard(&format!("{}::LogonNotify", self.tag()), || {
            // SAFETY: valid strings/pointer from TermService.
            let (u, d) = unsafe { (from_pcwstr(user.0), from_pcwstr(dom.0)) };
            let sid = if sessionid.is_null() { u32::MAX } else { unsafe { (*sessionid).SessionId } };
            logi!("{}::LogonNotify user='{d}\\{u}' sessionId={sid}", self.tag());
            // Docs: event notification, return immediately, no RDS API calls. We only wipe + send.
            self.wipe_credentials("LogonNotify");
            self.event(Event::Logon, sid, &format!("{d}\\{u}"));
            Ok(())
        })
    }

    fn PreDisconnect(&self, reason: u32) -> windows_core::Result<()> {
        guard(&format!("{}::PreDisconnect", self.tag()), || {
            logi!("{}::PreDisconnect reason={reason} (0x{reason:X})", self.tag());
            self.event(Event::PreDisconnect, reason, "");
            Ok(())
        })
    }

    fn DisconnectNotify(&self) -> windows_core::Result<()> {
        guard(&format!("{}::DisconnectNotify", self.tag()), || {
            self.drop_input("DisconnectNotify");
            self.event(Event::DisconnectNotify, 0, "");
            Ok(())
        })
    }

    fn Close(&self) -> windows_core::Result<()> {
        guard(&format!("{}::Close", self.tag()), || {
            self.closed.store(true, Ordering::SeqCst);
            self.wipe_credentials("Close");
            self.drop_input("Close");
            let channels = self.state.lock().map(|mut s| std::mem::take(&mut s.channels)).unwrap_or_default();
            for c in channels {
                c.shutdown();
            }
            self.event(Event::Closed, 0, "");
            let cb = {
                let mut st = self.state.lock().unwrap();
                self.release_driver_handle(&mut st);
                if let Some(w) = self.writer.lock().ok().as_ref().and_then(|w| w.as_ref()) {
                    w.shutdown(); // wakes the worker's blocking read
                }
                st.callback.take()
            };
            drop(cb); // releases the IWRdsProtocolConnectionCallback reference (docs: on close)
            Ok(())
        })
    }

    fn GetProtocolStatus(&self, p: *mut WTS_PROTOCOL_STATUS) -> windows_core::Result<()> {
        guard(&format!("{}::GetProtocolStatus", self.tag()), || {
            if p.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: TermService-owned output struct; all-zero is a valid WTS_PROTOCOL_STATUS.
            unsafe { std::ptr::write_bytes(p, 0, 1) };
            Ok(())
        })
    }

    fn GetLastInputTime(&self) -> windows_core::Result<u64> {
        // Milliseconds since last input. Reported as 0 ("just now"): idle-session policies are not
        // applied to FastRDP sessions.
        guard(&format!("{}::GetLastInputTime", self.tag()), || Ok(0))
    }

    fn SetErrorInfo(&self, ulerror: u32) -> windows_core::Result<()> {
        guard(&format!("{}::SetErrorInfo", self.tag()), || {
            logw!("{}::SetErrorInfo ulError={ulerror} (0x{ulerror:X})", self.tag());
            self.event(Event::SetErrorInfo, ulerror, "");
            Ok(())
        })
    }

    fn CreateVirtualChannel(&self, name: &PCSTR, bstatic: BOOL, prio: u32) -> windows_core::Result<usize> {
        guard(&format!("{}::CreateVirtualChannel", self.tag()), || {
            // SAFETY: NUL-terminated ANSI string from TermService.
            let n = if name.0.is_null() { String::new() } else { unsafe { name.to_string().unwrap_or_default() } };
            if !self.cfg.virtual_channels {
                logi!("{}::CreateVirtualChannel name='{n}' static={} priority={prio} -> E_NOTIMPL", self.tag(), bstatic.0);
                return Err(E_NOTIMPL.into());
            }
            let cid = self.next_channel.fetch_add(1, Ordering::Relaxed);
            match crate::channels::create(self.id, cid, &n, bstatic.as_bool(), self.sender()) {
                Ok((ch, h)) => {
                    logi!("{}::CreateVirtualChannel name='{n}' static={} priority={prio} -> handle {h:?}", self.tag(), bstatic.0);
                    if let Ok(mut s) = self.state.lock() {
                        s.channels.push(ch);
                    }
                    Ok(h as usize)
                }
                Err(e) => {
                    logw!("{}::CreateVirtualChannel name='{n}': {e}", self.tag());
                    Err(windows_core::HRESULT(0x80004005u32 as i32).into())
                }
            }
        })
    }

    fn QueryProperty(&self, qt: &GUID, nin: u32, nout: u32, _pin: *const WTS_PROPERTY_VALUE, _pout: *mut WTS_PROPERTY_VALUE) -> windows_core::Result<()> {
        guard(&format!("{}::QueryProperty", self.tag()), || {
            logi!("{}::QueryProperty type={} in={nin} out={nout} -> E_NOTIMPL", self.tag(), guid_str(qt));
            Err(E_NOTIMPL.into())
        })
    }

    fn GetShadowConnection(&self) -> windows_core::Result<IWRdsProtocolShadowConnection> {
        guard(&format!("{}::GetShadowConnection", self.tag()), || Err(E_NOTIMPL.into()))
    }

    fn NotifyCommandProcessCreated(&self, sessionid: u32) -> windows_core::Result<()> {
        guard(&format!("{}::NotifyCommandProcessCreated", self.tag()), || {
            logi!("{}::NotifyCommandProcessCreated sessionId={sessionid}", self.tag());
            self.event(Event::CommandProcessCreated, sessionid, "");
            Ok(())
        })
    }
}

impl IWRdsWddmIddProps_Impl for Connection_Impl {
    fn GetHardwareId(&self, buf: &PCWSTR, count: u32) -> windows_core::Result<()> {
        guard(&format!("{}::IWRdsWddmIddProps::GetHardwareId", self.tag()), || self.hardware_id(buf, count, "IWRdsWddmIddProps"))
    }

    fn OnDriverLoad(&self, sessionid: u32, driverhandle: HANDLE_PTR) -> windows_core::Result<()> {
        guard(&format!("{}::IWRdsWddmIddProps::OnDriverLoad", self.tag()), || {
            logi!("{}::OnDriverLoad(handle) sessionId={sessionid} handle=0x{:X} (ownership transferred)", self.tag(), driverhandle.0);
            let mut st = self.state.lock().unwrap();
            self.release_driver_handle(&mut st);
            st.driver_handle = driverhandle;
            drop(st);
            self.event(Event::DriverLoad, sessionid, "handle");
            Ok(())
        })
    }

    fn OnDriverUnload(&self, sessionid: u32) -> windows_core::Result<()> {
        guard(&format!("{}::OnDriverUnload", self.tag()), || {
            let mut st = self.state.lock().unwrap();
            self.release_driver_handle(&mut st);
            drop(st);
            self.event(Event::DriverUnload, sessionid, "");
            Ok(())
        })
    }

    fn EnableWddmIdd(&self, enabled: BOOL) -> windows_core::Result<()> {
        guard(&format!("{}::EnableWddmIdd", self.tag()), || {
            logi!("{}::EnableWddmIdd({})", self.tag(), enabled.0);
            self.state.lock().unwrap().wddm_idd = enabled.as_bool();
            self.event(Event::EnableWddmIdd, enabled.0 as u32, "");
            Ok(())
        })
    }
}

impl IWRdsWddmIddProps1_Impl for Connection_Impl {
    fn GetHardwareId(&self, buf: &PCWSTR, count: u32) -> windows_core::Result<()> {
        guard(&format!("{}::IWRdsWddmIddProps1::GetHardwareId", self.tag()), || self.hardware_id(buf, count, "IWRdsWddmIddProps1"))
    }

    fn OnDriverLoad(&self, sessionid: u32, deviceinstance: &PCWSTR) -> windows_core::Result<()> {
        guard(&format!("{}::IWRdsWddmIddProps1::OnDriverLoad", self.tag()), || {
            // SAFETY: NUL-terminated string from TermService (may be an interface path, per OpenStream notes).
            let inst = unsafe { from_pcwstr(deviceinstance.0) };
            logi!("{}::OnDriverLoad(deviceInstance) sessionId={sessionid} device='{inst}'", self.tag());
            self.event(Event::DriverLoad, sessionid, &inst);
            Ok(())
        })
    }

    fn OnDriverUnload(&self, sessionid: u32) -> windows_core::Result<()> {
        guard(&format!("{}::IWRdsWddmIddProps1::OnDriverUnload", self.tag()), || {
            self.event(Event::DriverUnload, sessionid, "");
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- license connection
#[implement(IWRdsProtocolLicenseConnection)]
pub struct LicenseConnection {
    conn_id: u32,
    client_name: String,
}

impl IWRdsProtocolLicenseConnection_Impl for LicenseConnection_Impl {
    fn RequestLicensingCapabilities(&self, caps: *mut WTS_LICENSE_CAPABILITIES, pcb: *mut u32) -> windows_core::Result<()> {
        guard(&format!("License#{}::RequestLicensingCapabilities", self.conn_id), || {
            if caps.is_null() || pcb.is_null() {
                return Err(E_POINTER.into());
            }
            // SAFETY: TermService-owned buffers (checked).
            unsafe {
                let in_cb = *pcb;
                let c = &mut *caps;
                c.KeyExchangeAlg = WTS_KEY_EXCHANGE_ALG_RSA;
                c.ProtocolVer = WTS_LICENSE_PROTOCOL_VERSION | WTS_LICENSE_PREAMBLE_VERSION;
                c.fAuthenticateServer = BOOL(0);
                c.CertType = WTS_CERT_TYPE_INVALID;
                let name: Vec<u8> = self.client_name.encode_utf16().take(20).flat_map(|u| u.to_le_bytes()).collect();
                c.rgbClientName = [0; 42];
                c.rgbClientName[..name.len()].copy_from_slice(&name);
                c.cbClientName = (name.len() + 2) as u32;
                *pcb = std::mem::size_of::<WTS_LICENSE_CAPABILITIES>() as u32;
                logi!("License#{}::RequestLicensingCapabilities cbIn={in_cb} cbOut={}", self.conn_id, *pcb);
            }
            Ok(())
        })
    }

    fn SendClientLicense(&self, _lic: *const u8, cb: u32) -> windows_core::Result<()> {
        guard(&format!("License#{}::SendClientLicense", self.conn_id), || {
            logi!("License#{}::SendClientLicense cb={cb}", self.conn_id);
            Ok(())
        })
    }

    fn RequestClientLicense(&self, _r1: *const u8, r2: u32, _lic: *mut u8, pcb: *mut u32) -> windows_core::Result<()> {
        guard(&format!("License#{}::RequestClientLicense", self.conn_id), || {
            // SAFETY: TermService-owned in/out count.
            let cap = if pcb.is_null() { 0 } else { unsafe { *pcb } };
            if !pcb.is_null() {
                unsafe { *pcb = 0 };
            }
            logi!("License#{}::RequestClientLicense reserve2={r2} capacity={cap} -> 0 bytes", self.conn_id);
            Ok(())
        })
    }

    fn ProtocolComplete(&self, complete: u32) -> windows_core::Result<()> {
        guard(&format!("License#{}::ProtocolComplete", self.conn_id), || {
            logi!("License#{}::ProtocolComplete ulComplete={complete}", self.conn_id);
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- logon error redirector
#[implement(IWRdsProtocolLogonErrorRedirector)]
pub struct LogonErrorRedirector {
    conn_id: u32,
}

const NOT_HANDLED: WTS_LOGON_ERROR_REDIRECTOR_RESPONSE = WTS_LOGON_ERR_NOT_HANDLED;

impl IWRdsProtocolLogonErrorRedirector_Impl for LogonErrorRedirector_Impl {
    fn OnBeginPainting(&self) -> windows_core::Result<()> {
        guard(&format!("Redirector#{}::OnBeginPainting", self.conn_id), || Ok(()))
    }

    fn RedirectStatus(&self, msg: &PCWSTR) -> windows_core::Result<WTS_LOGON_ERROR_REDIRECTOR_RESPONSE> {
        guard(&format!("Redirector#{}::RedirectStatus", self.conn_id), || {
            // SAFETY: NUL-terminated string from winlogon via TermService.
            logi!("Redirector#{}::RedirectStatus '{}'", self.conn_id, unsafe { from_pcwstr(msg.0) });
            Ok(NOT_HANDLED)
        })
    }

    fn RedirectMessage(&self, cap: &PCWSTR, msg: &PCWSTR, utype: u32) -> windows_core::Result<WTS_LOGON_ERROR_REDIRECTOR_RESPONSE> {
        guard(&format!("Redirector#{}::RedirectMessage", self.conn_id), || {
            // SAFETY: NUL-terminated strings.
            let (c, m) = unsafe { (from_pcwstr(cap.0), from_pcwstr(msg.0)) };
            logi!("Redirector#{}::RedirectMessage caption='{c}' message='{m}' type=0x{utype:X}", self.conn_id);
            Ok(NOT_HANDLED)
        })
    }

    fn RedirectLogonError(&self, status: i32, sub: i32, cap: &PCWSTR, msg: &PCWSTR, utype: u32) -> windows_core::Result<WTS_LOGON_ERROR_REDIRECTOR_RESPONSE> {
        guard(&format!("Redirector#{}::RedirectLogonError", self.conn_id), || {
            // SAFETY: NUL-terminated strings.
            let (c, m) = unsafe { (from_pcwstr(cap.0), from_pcwstr(msg.0)) };
            logw!(
                "Redirector#{}::RedirectLogonError ntStatus=0x{:08X} subStatus=0x{:08X} caption='{c}' message='{m}' type=0x{utype:X}",
                self.conn_id,
                status as u32,
                sub as u32
            );
            Ok(NOT_HANDLED)
        })
    }
}

/// Writes one message on the shared transport (2 s write timeout; never blocks TermService long).
fn send_on(w: &Mutex<Option<Box<dyn Transport>>>, m: &Message) -> bool {
    let Ok(mut g) = w.lock() else { return false };
    let Some(t) = g.as_mut() else { return false };
    let ok = write_message(t, m).is_ok();
    let _ = t.flush();
    ok
}
