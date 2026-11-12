//! IWRdsProtocolListener + the pipe accept loop and per-client worker threads.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use protocol::{Event, EventMsg, HelloAck, Message, read_message, write_message};
use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::System::RemoteDesktop::*;
use windows_core::{ComObject, Ref, implement};

use crate::config::ListenerConfig;
use crate::connection::{Agile, ClientInfo, Connection, Credentials};
use crate::transport::{PipeListener, Transport, pipe_sddl};
use crate::util::{E_FAIL, E_NOTIMPL, E_POINTER, current_session_id, guard, hr_of, os_summary};
use crate::{loge, logi, logw};

struct Running {
    stop: Arc<AtomicBool>,
    accept_thread: Option<JoinHandle<()>>,
    callback: Agile<IWRdsProtocolListenerCallback>,
}

#[implement(IWRdsProtocolListener)]
pub struct Listener {
    cfg: ListenerConfig,
    running: Mutex<Option<Running>>,
}

impl Listener {
    pub fn new(cfg: ListenerConfig) -> Self {
        logi!("Listener '{}': COM object created", cfg.listener_name);
        Listener { cfg, running: Mutex::new(None) }
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        logi!("Listener '{}': COM object destroyed", self.cfg.listener_name);
    }
}

impl IWRdsProtocolListener_Impl for Listener_Impl {
    fn GetSettings(&self, level: WRDS_LISTENER_SETTING_LEVEL) -> windows_core::Result<WRDS_LISTENER_SETTINGS> {
        guard(&format!("Listener::GetSettings(level={})", level.0), || Err(E_NOTIMPL.into()))
    }

    fn StartListen(&self, pcallback: Ref<IWRdsProtocolListenerCallback>) -> windows_core::Result<()> {
        guard(&format!("Listener('{}')::StartListen", self.cfg.listener_name), || {
            // Docs: the protocol must add a reference to the callback and release it in StopListen.
            let cb = pcallback.cloned().ok_or(windows_core::Error::from(E_POINTER))?;
            let mut running = self.running.lock().unwrap();
            if running.is_some() {
                logw!("StartListen called while already listening - ignoring");
                return Ok(());
            }
            let stop = Arc::new(AtomicBool::new(false));
            let cfg = self.cfg.clone();
            let cb_thread = Agile(cb.clone());
            let stop_thread = stop.clone();
            let name = self.cfg.pipe_name();
            let listener = PipeListener::new(&name, &pipe_sddl());
            let t = std::thread::Builder::new()
                .name("fastrdp-accept".into())
                .spawn(move || accept_loop_pipe(cfg, listener, cb_thread, stop_thread))
                .map_err(|_| windows_core::Error::from(E_FAIL))?;
            logi!("Listener '{}': listening on pipe {name}", self.cfg.listener_name);
            *running = Some(Running { stop, accept_thread: Some(t), callback: Agile(cb) });
            Ok(())
        })
    }

    fn StopListen(&self) -> windows_core::Result<()> {
        guard(&format!("Listener('{}')::StopListen", self.cfg.listener_name), || {
            let r = self.running.lock().unwrap().take();
            if let Some(mut r) = r {
                r.stop.store(true, Ordering::SeqCst);
                if let Some(t) = r.accept_thread.take() {
                    let _ = t.join(); // accept loop polls the stop flag every 200 ms
                }
                drop(r.callback); // release the reference taken in StartListen
            }
            Ok(())
        })
    }
}

fn spawn_worker(cfg: ListenerConfig, stream: Box<dyn Transport>, peer: PeerInfo, cb: Agile<IWRdsProtocolListenerCallback>) {
    let label = peer.label.clone();
    let r = std::thread::Builder::new().name("fastrdp-conn".into()).spawn(move || {
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| connection_worker(cfg, stream, peer, cb)));
        if res.is_err() {
            loge!("connection worker for {label} panicked (caught)");
        }
    });
    if let Err(e) = r {
        loge!("failed to spawn connection worker: {e}");
    }
}

fn accept_loop_pipe(cfg: ListenerConfig, mut listener: PipeListener, cb: Agile<IWRdsProtocolListenerCallback>, stop: Arc<AtomicBool>) {
    logi!("accept loop (pipe {}) started", cfg.pipe_name());
    while !stop.load(Ordering::SeqCst) {
        match listener.accept(&stop) {
            Ok(Some(p)) => {
                let label = format!("pipe-client-pid-{}", p.client_pid);
                logi!("pipe connection from pid {}", p.client_pid);
                // Pipe peers are local and restricted by the pipe DACL (SYSTEM / LOCAL SERVICE / Administrators).
                let info = PeerInfo { label, ip: "127.0.0.1".into(), is_v6: false };
                spawn_worker(cfg.clone(), Box::new(p), info, Agile(cb.0.clone()));
            }
            Ok(None) => break,
            Err(e) => {
                logw!("pipe accept failed: {e}");
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    logi!("accept loop (pipe) stopped");
}

struct PeerInfo {
    label: String,
    ip: String,
    is_v6: bool,
}

/// COM must be initialised (MTA) on threads that call TermService's callbacks.
struct ComMta;
impl ComMta {
    fn init() -> Option<ComMta> {
        // SAFETY: balanced with CoUninitialize in Drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok().ok().map(|_| ComMta) }
    }
}
impl Drop for ComMta {
    fn drop(&mut self) {
        // SAFETY: paired with successful CoInitializeEx.
        unsafe { CoUninitialize() };
    }
}

fn connection_worker(cfg: ListenerConfig, mut stream: Box<dyn Transport>, info: PeerInfo, cb: Agile<IWRdsProtocolListenerCallback>) {
    let peer = info.label.clone();
    stream.set_read_timeout(Some(Duration::from_secs(10)));
    stream.set_write_timeout(Some(Duration::from_secs(2)));

    // 1. HELLO
    let hello = match read_message(&mut stream) {
        Ok(Message::Hello(h)) => h,
        Ok(other) => {
            logw!("{peer}: expected HELLO, got type 0x{:04X}", other.type_code());
            return;
        }
        Err(e) => {
            logw!("{peer}: HELLO read failed: {e}");
            return;
        }
    };
    logi!(
        "{peer}: HELLO client='{}' {}x{}@{} performance flags 0x{:X}",
        hello.client_name,
        hello.width,
        hello.height,
        hello.refresh_hz,
        hello.performance_flags
    );
    let ack = Message::HelloAck(HelloAck {
        version: protocol::VERSION,
        mode: 1, // a session is created for every authenticated connection
        // SAFETY: trivial Win32 call.
        server_pid: unsafe { windows_sys::Win32::System::Threading::GetCurrentProcessId() },
        server_session_id: current_session_id(),
        listener: cfg.listener_name.clone(),
        os_info: format!("{} termsrv={}", os_summary(), crate::util::termsrv_version()),
    });
    if write_message(&mut stream, &ack).is_err() {
        return;
    }

    // 2. AUTH
    stream.set_read_timeout(Some(Duration::from_secs(60)));
    let auth = match read_message(&mut stream) {
        Ok(Message::Auth(a)) => a,
        Ok(Message::Bye) => {
            logi!("{peer}: BYE before AUTH");
            return;
        }
        Ok(other) => {
            logw!("{peer}: expected AUTH, got type 0x{:04X}", other.type_code());
            return;
        }
        Err(e) => {
            logw!("{peer}: AUTH read failed: {e}");
            return;
        }
    };
    logi!(
        "{peer}: CONNECTION REQUEST user='{}\\{}' password=<redacted, {}>",
        auth.domain,
        auth.user,
        if auth.password.is_empty() { "empty" } else { "present" }
    );

    // 3. check the credentials before a session is created (like RDP's NLA), see preauth.rs
    match crate::preauth::check(&auth.domain, &auth.user, auth.password.as_utf16()) {
        crate::preauth::Verdict::Ok => logi!("{peer}: credentials verified"),
        crate::preauth::Verdict::Reject(code, msg) => {
            logw!("{peer}: sign-in rejected before creating a session (error {code}): {msg}");
            let _ = write_message(&mut stream, &Message::Event(EventMsg { event: Event::Rejected as u32, value: code, text: msg.into() }));
            return; // `auth` dropped here -> Secret wiped
        }
        crate::preauth::Verdict::Undecided(code) => logw!("{peer}: credentials could not be checked up front (error {code}) - Windows' logon decides"),
    }

    // 4. hand the connection to TermService.
    let Some(_com) = ComMta::init() else {
        loge!("{peer}: CoInitializeEx(MTA) failed");
        return;
    };
    let writer = match stream.clone_box() {
        Ok(w) => w,
        Err(e) => {
            loge!("{peer}: socket clone failed: {e}");
            return;
        }
    };
    let client = ClientInfo {
        width: hello.width,
        height: hello.height,
        refresh_hz: hello.refresh_hz,
        client_name: hello.client_name.clone(),
        peer_ip: info.ip.clone(),
        peer_is_v6: info.is_v6,
        performance_flags: hello.performance_flags,
    };
    let creds = Credentials { domain: auth.domain.clone(), user: auth.user.clone(), password: auth.password.clone() };
    drop(auth);
    let conn = ComObject::new(Connection::new(cfg.clone(), client, writer, creds));
    let tag = format!("Connection#{}", conn.id);

    // Same settings as Microsoft's sample: "Connections will fail if these aren't set".
    // SAFETY: all-zero is a valid WRDS_CONNECTION_SETTINGS.
    let mut settings: WRDS_CONNECTION_SETTINGS = unsafe { std::mem::zeroed() };
    settings.WRdsConnectionSettingLevel = WRDS_CONNECTION_SETTING_LEVEL_1;
    // SAFETY: writing the level-1 union arm we just selected.
    unsafe {
        let s1 = &mut settings.WRdsConnectionSetting.WRdsConnectionSettings1;
        s1.WRdsListenerSettings.WRdsListenerSettingLevel = WRDS_LISTENER_SETTING_LEVEL_1;
        s1.WRdsListenerSettings.WRdsListenerSetting.WRdsListenerSettings1.pSecurityDescriptor = std::ptr::null_mut();
    }

    let iconn: IWRdsProtocolConnection = conn.to_interface();
    logi!("{tag}: calling IWRdsProtocolListenerCallback::OnConnected");
    // SAFETY: valid interfaces; settings outlive the call.
    let r = unsafe { cb.0.OnConnected(&iconn, &settings) };
    let hr = hr_of(&r);
    logi!("{tag}: OnConnected -> hr=0x{hr:08X}");
    conn.event(Event::OnConnectedResult, hr, "");
    let conn_cb = match r {
        Ok(c) => c,
        Err(_) => {
            conn.abort_before_accept();
            conn.detach_socket();
            return;
        }
    };
    // SAFETY: valid callback returned by TermService.
    match unsafe { conn_cb.GetConnectionId() } {
        Ok(id) => {
            logi!("{tag}: GetConnectionId -> {id}");
            conn.event(Event::ConnectionId, id, "");
        }
        Err(e) => logw!("{tag}: GetConnectionId failed hr=0x{:08X}", e.code().0 as u32),
    }
    conn.attach_callback(conn_cb.clone());
    // SAFETY: valid callback.
    let r = unsafe { conn_cb.OnReady() };
    let hr = hr_of(&r);
    logi!("{tag}: IWRdsProtocolConnectionCallback::OnReady -> hr=0x{hr:08X}");
    conn.event(Event::OnReadyResult, hr, "");
    drop(conn_cb);
    drop(iconn);

    // 4. Keep the transport open until the client leaves or TermService closes us.
    stream.set_read_timeout(None);
    let mut channels = protocol::ChannelAssembler::default();
    let why = loop {
        match read_message(&mut stream) {
            Ok(Message::Bye) => break "client BYE".to_string(),
            Ok(Message::Input(e)) => conn.inject(&e),
            Ok(Message::ChannelData(d)) => {
                if let Some((id, msg)) = channels.push(d) {
                    conn.channel_write(id, &msg);
                }
            }
            Ok(m) => logi!("{tag}: ignoring client message type 0x{:04X}", m.type_code()),
            Err(e) => break if conn.closed_by_service() { "closed by TermService".into() } else { format!("read error: {e}") },
        }
    };
    conn.on_transport_closed(&why);
    conn.detach_socket();
    logi!("{tag}: worker exiting");
}
