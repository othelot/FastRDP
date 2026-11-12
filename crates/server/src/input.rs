//! Input relay. Preferred path: the provider's session keyboard/mouse devices (like RDP, via the
//! provider pipe, coordinates normalized to 0..=65535). Fallback: session agents
//! (fastrdp-agent.exe, started by Task Scheduler inside every interactive session) connected to
//! `\\.\pipe\FastRDP.Input`; the server binds each pipe client to the session it really runs in
//! (`GetNamedPipeClientSessionId`), so a user in another session can never receive or inject
//! someone else's input. Client input for session N goes only to session N.
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use protocol::asyncio::{read_message, write_message};
use protocol::{InputEvent, Message};
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};
use tokio::sync::mpsc;

use crate::{info, warn};

pub const PIPE: &str = r"\\.\pipe\FastRDP.Input";
/// SYSTEM, LOCAL SERVICE (this service) full; INTERACTIVE users read/write (agents); Administrators.
const SDDL: &str = "D:P(A;;GA;;;SY)(A;;GA;;;LS)(A;;GRGW;;;IU)(A;;GA;;;BA)";

struct Agent {
    tx: mpsc::UnboundedSender<Message>,
    pid: u32,
    generation: u64,
}

#[derive(Clone, Default)]
pub struct Agents {
    inner: Arc<Mutex<(u64, HashMap<u32, Agent>)>>,
}

impl Agents {
    /// Forwards an input event to the agent of `session`. Returns false if there is none.
    pub fn send(&self, session: u32, e: InputEvent) -> bool {
        let g = self.inner.lock().unwrap();
        match g.1.get(&session) {
            Some(a) => a.tx.send(Message::Input(e)).is_ok(),
            None => false,
        }
    }
    pub fn has(&self, session: u32) -> bool {
        self.inner.lock().unwrap().1.contains_key(&session)
    }
    fn register(&self, session: u32, pid: u32, tx: mpsc::UnboundedSender<Message>) -> u64 {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        let generation = g.0;
        if let Some(old) = g.1.insert(session, Agent { tx, pid, generation }) {
            info!("input agent for session {session}: pid {} replaced by pid {pid}", old.pid);
        }
        generation
    }
    fn unregister(&self, session: u32, generation: u64) {
        let mut g = self.inner.lock().unwrap();
        if g.1.get(&session).is_some_and(|a| a.generation == generation) {
            g.1.remove(&session);
        }
    }
}

fn client_session(pipe: &NamedPipeServer) -> Option<u32> {
    use std::os::windows::io::AsRawHandle;
    let mut sid = 0u32;
    // SAFETY: valid connected pipe handle, valid out pointer.
    let ok = unsafe { windows_sys::Win32::System::Pipes::GetNamedPipeClientSessionId(pipe.as_raw_handle() as _, &mut sid) };
    (ok != 0).then_some(sid)
}

fn create_instance(first: bool) -> std::io::Result<NamedPipeServer> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Authorization::{ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1};
    use windows_sys::Win32::Security::SECURITY_ATTRIBUTES;
    let sddl: Vec<u16> = SDDL.encode_utf16().chain(Some(0)).collect();
    // SAFETY: SD allocated by the API, freed after the pipe instance is created.
    unsafe {
        let mut sd = std::ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(sddl.as_ptr(), SDDL_REVISION_1, &mut sd, std::ptr::null_mut()) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut sa = SECURITY_ATTRIBUTES { nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32, lpSecurityDescriptor: sd, bInheritHandle: 0 };
        let r = ServerOptions::new()
            .first_pipe_instance(first)
            .reject_remote_clients(true)
            .create_with_security_attributes_raw(PIPE, &mut sa as *mut _ as *mut std::ffi::c_void);
        LocalFree(sd);
        r
    }
}

/// Accept loop for session agents (runs for the lifetime of the service).
pub async fn serve(agents: Agents) {
    let mut first = true;
    loop {
        let server = match create_instance(first) {
            Ok(s) => s,
            Err(e) => {
                warn!("input pipe {PIPE}: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };
        first = false;
        if let Err(e) = server.connect().await {
            warn!("input pipe connect: {e}");
            continue;
        }
        let agents = agents.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_agent(server, agents).await {
                info!("input agent ended: {e}");
            }
        });
    }
}

async fn handle_agent(pipe: NamedPipeServer, agents: Agents) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let Some(session) = client_session(&pipe) else { return Err("GetNamedPipeClientSessionId failed".into()) };
    let (mut r, mut w) = tokio::io::split(pipe);
    let hello = match tokio::time::timeout(Duration::from_secs(5), read_message(&mut r)).await?? {
        Message::AgentHello(h) => h,
        m => return Err(format!("expected AGENT_HELLO, got 0x{:04x}", m.type_code()).into()),
    };
    if hello.session_id != session {
        warn!("input agent pid {} claims session {} but runs in session {session} - rejected", hello.pid, hello.session_id);
        return Ok(());
    }
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();
    let generation = agents.register(session, hello.pid, tx);
    info!("input agent registered: session {session} pid {}", hello.pid);
    // writer: forward events; reader: detect the agent going away
    let res = tokio::select! {
        r = async {
            while let Some(m) = rx.recv().await {
                write_message(&mut w, &m).await?;
            }
            Ok::<(), protocol::WireError>(())
        } => r.map_err(|e| e.to_string()),
        r = read_message(&mut r) => Err(format!("agent closed: {}", r.err().map(|e| e.to_string()).unwrap_or_default())),
    };
    agents.unregister(session, generation);
    info!("input agent for session {session} gone ({})", res.err().unwrap_or_default());
    Ok(())
}

/// Where input of one client connection goes.
#[derive(Clone)]
pub struct Route {
    pub session: tokio::sync::watch::Receiver<Option<u32>>,
    /// session whose provider devices are ready (`Event::InputDevices`)
    pub devices: tokio::sync::watch::Receiver<Option<u32>>,
    pub provider: Option<mpsc::UnboundedSender<Message>>,
    pub desktop: Arc<std::sync::atomic::AtomicU64>,
    pub agents: Agents,
}

/// Remote pixel -> 0..=65535 (absolute mouse coordinates of the session desktop).
pub fn normalize(v: u16, size: u32) -> u16 {
    if size <= 1 {
        return 0;
    }
    ((v.min((size - 1) as u16) as u32 * 65535 + (size - 1) / 2) / (size - 1)) as u16
}

impl Route {
    /// Current path for the session (see `protocol::input_path`).
    pub fn path(&self) -> (Option<u32>, u32) {
        let sid = *self.session.borrow();
        let p = match sid {
            Some(s) if self.provider.is_some() && *self.devices.borrow() == Some(s) => protocol::input_path::DEVICES,
            Some(s) if self.agents.has(s) => protocol::input_path::AGENT,
            _ => protocol::input_path::NONE,
        };
        (sid, p)
    }

    pub fn send(&self, e: InputEvent) -> bool {
        match self.path() {
            (Some(_), protocol::input_path::DEVICES) => {
                let d = self.desktop.load(std::sync::atomic::Ordering::Relaxed);
                let (w, h) = if d == 0 { (1920, 1080) } else { ((d >> 32) as u32, d as u32) };
                let n = |x: u16, y: u16| (normalize(x, w), normalize(y, h));
                let e = match e {
                    InputEvent::MouseMove { x, y } => {
                        let (x, y) = n(x, y);
                        InputEvent::MouseMove { x, y }
                    }
                    InputEvent::Button { button, down, x, y } => {
                        let (x, y) = n(x, y);
                        InputEvent::Button { button, down, x, y }
                    }
                    InputEvent::Wheel { delta, horizontal, x, y } => {
                        let (x, y) = n(x, y);
                        InputEvent::Wheel { delta, horizontal, x, y }
                    }
                    other => other,
                };
                self.provider.as_ref().is_some_and(|p| p.send(Message::Input(e)).is_ok())
            }
            (Some(s), protocol::input_path::AGENT) => self.agents.send(s, e),
            _ => false,
        }
    }
}

/// Per client connection: reliable input stream + mouse-move datagrams -> session devices / agent.
pub async fn relay_client(conn: quinn::Connection, route: Route) {
    let remote = conn.remote_address();
    let input_stream = {
        let (conn, route) = (conn.clone(), route.clone());
        async move {
            // client uni streams: input (keys, buttons, wheel) and virtual-channel data, each on its own
            // stream so a large clipboard / file transfer never delays input
            let mut readers = tokio::task::JoinSet::new();
            while let Ok(mut s) = conn.accept_uni().await {
                if readers.len() >= 8 {
                    warn!("[{remote}] too many client streams - ignoring one");
                    continue;
                }
                let route = route.clone();
                readers.spawn(async move {
                    let mut dropped = 0u64;
                    while let Ok(m) = read_message(&mut s).await {
                        match m {
                            Message::Input(e) => {
                                if !route.send(e) {
                                    dropped += 1;
                                    if dropped == 1 || dropped.is_multiple_of(500) {
                                        warn!("[{remote}] no input path for session {:?} - {dropped} input event(s) dropped", *route.session.borrow());
                                    }
                                }
                            }
                            Message::ChannelData(d) => {
                                if let Some(p) = &route.provider {
                                    let _ = p.send(Message::ChannelData(d));
                                }
                            }
                            _ => {}
                        }
                    }
                });
            }
            readers.shutdown().await;
        }
    };
    let mouse = {
        let (conn, route) = (conn.clone(), route.clone());
        async move {
            let mut last: Option<u32> = None;
            while let Ok(d) = conn.read_datagram().await {
                let Some((seq, x, y)) = protocol::mouse_datagram::decode(&d) else { continue };
                if last.is_some_and(|l| (seq.wrapping_sub(l) as i32) <= 0) {
                    continue; // older than what we already applied
                }
                last = Some(seq);
                route.send(InputEvent::MouseMove { x, y });
            }
        }
    };
    tokio::join!(input_stream, mouse);
    route.send(InputEvent::ReleaseAll);
}

#[cfg(test)]
mod tests {
    #[test]
    fn normalize() {
        assert_eq!(super::normalize(0, 1920), 0);
        assert_eq!(super::normalize(1919, 1920), 65535);
        assert_eq!(super::normalize(5000, 1920), 65535);
        assert_eq!(super::normalize(960, 1920), 32785);
        assert_eq!(super::normalize(3, 1), 0);
    }
}
