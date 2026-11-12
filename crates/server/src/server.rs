//! QUIC endpoint. Per client connection:
//!   control (bidirectional stream, protocol framing): HELLO, AUTH -> relayed to the provider pipe;
//!   provider replies (HELLO_ACK, events incl. SessionId) are relayed back to the client;
//!   SessionId(n) starts the video thread for session n (datagrams); client FEEDBACK drives IDR/FEC.
//! Closing either side tears the other down (pipe EOF -> provider -> BrokenConnection).
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use protocol::asyncio::{read_message, write_message};
use protocol::{Event, EventMsg, Message};
use tokio::sync::{mpsc, watch};

use crate::input::{self, Agents};
use crate::video::{self, Feedback, VideoParams};
use crate::{info, warn};

pub const ALPN: &[u8] = b"fastrdp/1";

#[derive(Clone, Debug)]
pub struct Config {
    pub bind: SocketAddr,
    pub listener: String,
    pub fec_base: f32,
    pub data_dir: PathBuf,
}

pub async fn run(cfg: Config, mut shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
    let (cert, key, fp) = crate::cert::load_or_create(&cfg.data_dir.join("server"))?;
    info!("server certificate sha256 {fp}");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut tls = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(std::io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(vec![cert], key)
        .map_err(std::io::Error::other)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let qsc = quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(std::io::Error::other)?;
    let mut sc = quinn::ServerConfig::with_crypto(Arc::new(qsc));
    let mut tc = quinn::TransportConfig::default();
    tc.max_idle_timeout(Some(Duration::from_secs(10).try_into().unwrap()));
    tc.keep_alive_interval(Some(Duration::from_secs(2)));
    tc.datagram_send_buffer_size(8 << 20);
    tc.datagram_receive_buffer_size(Some(8 << 20));
    sc.transport_config(Arc::new(tc));
    let ep = quinn::Endpoint::server(sc, cfg.bind)?;
    info!("listening on udp {} (QUIC, ALPN fastrdp/1), provider pipe listener '{}'", cfg.bind, cfg.listener);
    let cfg = Arc::new(cfg);
    let agents = Agents::default();
    tokio::spawn(input::serve(agents.clone()));
    loop {
        tokio::select! {
            inc = ep.accept() => {
                let Some(inc) = inc else { break };
                let (cfg, agents) = (cfg.clone(), agents.clone());
                tokio::spawn(async move {
                    let remote = inc.remote_address();
                    match handle(cfg, inc, agents).await {
                        Ok(()) => info!("[{remote}] connection finished"),
                        Err(e) => warn!("[{remote}] connection ended: {e}"),
                    }
                });
            }
            _ = shutdown.changed() => break,
        }
    }
    ep.close(0u32.into(), b"shutdown");
    ep.wait_idle().await;
    Ok(())
}

type BoxErr = Box<dyn std::error::Error + Send + Sync>;

async fn open_provider_pipe(listener: &str) -> Result<tokio::net::windows::named_pipe::NamedPipeClient, BoxErr> {
    let name = format!(r"\\.\pipe\FastRDP.Provider.{listener}");
    for _ in 0..50 {
        match tokio::net::windows::named_pipe::ClientOptions::new().open(&name) {
            Ok(c) => return Ok(c),
            // ERROR_PIPE_BUSY / ERROR_FILE_NOT_FOUND: the provider is between two pipe instances
            Err(e) if matches!(e.raw_os_error(), Some(231) | Some(2)) => tokio::time::sleep(Duration::from_millis(100)).await,
            Err(e) => return Err(format!("open {name}: {e}").into()),
        }
    }
    Err(format!("{name}: busy").into())
}

async fn handle(cfg: Arc<Config>, inc: quinn::Incoming, agents: Agents) -> Result<(), BoxErr> {
    let conn = inc.await?;
    let remote = conn.remote_address();
    info!("[{remote}] QUIC connection established (max datagram {:?})", conn.max_datagram_size());
    let (mut send, mut recv) = tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await??;
    let hello = match tokio::time::timeout(Duration::from_secs(10), read_message(&mut recv)).await?? {
        Message::Hello(h) => h,
        m => return Err(format!("expected HELLO, got 0x{:04x}", m.type_code()).into()),
    };
    let auth = match tokio::time::timeout(Duration::from_secs(60), read_message(&mut recv)).await?? {
        Message::Auth(a) => a,
        m => return Err(format!("expected AUTH, got 0x{:04x}", m.type_code()).into()),
    };
    info!(
        "[{remote}] HELLO '{}' {}x{}@{} perf 0x{:X} AUTH user='{}\\{}' password=<redacted>",
        hello.client_name, hello.width, hello.height, hello.refresh_hz, hello.performance_flags, auth.domain, auth.user
    );

    // control writer task
    let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<Message>();
    let writer = tokio::spawn(async move {
        while let Some(m) = ctrl_rx.recv().await {
            if write_message(&mut send, &m).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });

    let fb = Arc::new(Feedback::default());
    let stop = Arc::new(AtomicBool::new(false));
    let (session_tx, mut session_rx) = watch::channel::<Option<u32>>(None);
    let (devices_tx, devices_rx) = watch::channel::<Option<u32>>(None);
    // remote display mode wanted by the client (initial: HELLO size; later: Display messages)
    let initial_display = protocol::DisplayRequest {
        width: hello.width as u32,
        height: hello.height as u32,
        refresh_hz: hello.refresh_hz as u32,
        scale_percent: 100,
        bitrate_kbps: 0,
    };
    let (display_tx, mut display_rx) = watch::channel(initial_display);
    // refresh rate for the video thread (follows the client's display requests)
    let refresh_hz = Arc::new(std::sync::atomic::AtomicU32::new(initial_display.clamped(frame_shm::MAX_WIDTH, frame_shm::MAX_HEIGHT).refresh_hz));
    let bitrate_kbps = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let mut display_ctl: Option<(u32, frame_shm::DisplayControl, Option<protocol::DisplayRequest>, u32)> = None;
    let desktop = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // provider side
    let provider_task = {
        let pipe = open_provider_pipe(&cfg.listener).await?;
        let (mut pr, mut pw) = tokio::io::split(pipe);
        let mut hello_fwd = hello.clone();
        hello_fwd.client_name = format!("{}@{}", hello.client_name, remote);
        write_message(&mut pw, &Message::Hello(hello_fwd)).await?;
        write_message(&mut pw, &Message::Auth(auth)).await?; // `auth` (Secret) is wiped on drop
        // provider pipe writer (input -> session devices); ends when every sender is dropped,
        // which closes the write half
        let (prov_tx, mut prov_rx) = mpsc::unbounded_channel::<Message>();
        let prov_writer = tokio::spawn(async move {
            while let Some(m) = prov_rx.recv().await {
                if write_message(&mut pw, &m).await.is_err() {
                    break;
                }
            }
            pw
        });
        // virtual channels (provider -> client) on their own uni stream, opened on first use
        let (chan_tx, mut chan_rx) = mpsc::unbounded_channel::<Message>();
        let chan_conn = conn.clone();
        tokio::spawn(async move {
            let mut stream: Option<quinn::SendStream> = None;
            while let Some(m) = chan_rx.recv().await {
                if stream.is_none() {
                    match chan_conn.open_uni().await {
                        Ok(s) => stream = Some(s),
                        Err(_) => break,
                    }
                }
                if write_message(stream.as_mut().unwrap(), &m).await.is_err() {
                    break;
                }
            }
            if let Some(mut s) = stream {
                let _ = s.finish();
            }
        });
        let ctrl = ctrl_tx.clone();
        let dev_tx = devices_tx.clone();
        let rx_task = tokio::spawn(async move {
            loop {
                match read_message(&mut pr).await {
                    Ok(m @ (Message::ChannelOpen(_) | Message::ChannelData(_) | Message::ChannelClose(_))) => {
                        if let Message::ChannelOpen(c) = &m {
                            info!("virtual channel #{} '{}' ({}) opened by the session", c.id, c.name, if c.is_static { "static" } else { "dynamic" });
                        }
                        let _ = chan_tx.send(m);
                    }
                    Ok(m) => {
                        if let Message::Event(e) = &m {
                            if e.event == Event::SessionId as u32 {
                                let _ = session_tx.send(Some(e.value));
                            }
                            if e.event == Event::InputDevices as u32 {
                                let _ = dev_tx.send(if e.value == 1 { e.text.parse().ok() } else { None });
                            }
                            if e.event == Event::DisconnectNotify as u32 {
                                let _ = dev_tx.send(None);
                            }
                            let closed = e.event == Event::Closed as u32 || e.event == Event::LogOnlyMode as u32 || e.event == Event::Rejected as u32;
                            let _ = ctrl.send(m.clone());
                            if closed {
                                break;
                            }
                            continue;
                        }
                        let _ = ctrl.send(m);
                    }
                    Err(e) => {
                        info!("provider pipe closed: {e}");
                        break;
                    }
                }
            }
        });
        Some((rx_task, prov_writer, prov_tx))
    };

    // client feedback reader
    let fb2 = fb.clone();
    let mut client_reader = tokio::spawn(async move {
        loop {
            match read_message(&mut recv).await {
                Ok(Message::Feedback(f)) => {
                    if let Some(p) = (f.frames_lost * 1000).checked_div(f.frames_complete + f.frames_lost) {
                        fb2.loss_permille.store(p, Ordering::Relaxed);
                    }
                    if f.request_idr != 0 {
                        fb2.idr_requested.store(true, Ordering::SeqCst);
                    }
                }
                Ok(Message::Display(d)) => {
                    let _ = display_tx.send(d);
                }
                Ok(Message::Bye) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    // keyboard/mouse: client input stream + mouse datagrams -> session devices (provider) or agent
    let route = input::Route {
        session: session_rx.clone(),
        devices: devices_rx.clone(),
        provider: provider_task.as_ref().map(|(_, _, tx)| tx.clone()),
        desktop: desktop.clone(),
        agents: agents.clone(),
    };
    let input_task = tokio::spawn(input::relay_client(conn.clone(), route.clone()));
    let mut input_tick = tokio::time::interval(Duration::from_millis(500));
    let mut input_state: Option<(u32, u32)> = None;
    // the provider reports session devices shortly after the session id: do not announce the
    // fallback path in between (the client would see devices -> agent -> devices)
    let mut session_changed = std::time::Instant::now();

    // video thread(s): (re)start when the session id is known / changes (reconnect to an existing session)
    let mut video: Option<(Arc<AtomicBool>, tokio::task::JoinHandle<()>)> = None;
    let frame_ids = Arc::new(std::sync::atomic::AtomicU32::new(1)); // continuous across session switches
    // The provider reader task owns the pipe's read half: it must be aborted on teardown, otherwise the
    // pipe stays open and the provider never sees EOF (session would stay Active without a client).
    let provider_abort = provider_task.as_ref().map(|(rx, _, _)| rx.abort_handle());
    let mut provider_done = Box::pin(async {
        match provider_task {
            Some((rx, writer, tx)) => {
                let _ = rx.await;
                // dropping the last sender ends the writer, which drops the pipe write half
                drop(tx);
                writer.abort();
            }
            None => std::future::pending::<()>().await,
        }
    });
    loop {
        tokio::select! {
            r = session_rx.changed() => {
                if r.is_err() { continue; }
                let Some(sid) = *session_rx.borrow() else { continue };
                session_changed = std::time::Instant::now();
                // the display mode must be in place before the session's driver starts
                apply_display(&remote, sid, *display_rx.borrow(), &mut display_ctl);
                if let Some((s, _)) = video.take() { s.store(true, Ordering::SeqCst); }
                let s = Arc::new(AtomicBool::new(false));
                let (c, ctrl, fbv, sv) = (conn.clone(), ctrl_tx.clone(), fb.clone(), s.clone());
                let params = VideoParams { frame_ids: frame_ids.clone(), desktop: desktop.clone(), session: sid, fec_base: cfg.fec_base, refresh_hz: refresh_hz.clone(), bitrate_kbps: bitrate_kbps.clone() };
                info!("[{remote}] session {sid}: starting video");
                video = Some((s, tokio::task::spawn_blocking(move || video::run(c, params, ctrl, fbv, sv))));
            }
            r = display_rx.changed() => {
                if r.is_err() { continue; }
                let want = display_rx.borrow().clamped(frame_shm::MAX_WIDTH, frame_shm::MAX_HEIGHT);
                refresh_hz.store(want.refresh_hz, Ordering::Relaxed);
                bitrate_kbps.store(want.bitrate_kbps, Ordering::Relaxed);
                if let Some(sid) = *session_rx.borrow() {
                    apply_display(&remote, sid, *display_rx.borrow(), &mut display_ctl);
                }
            }
            _ = input_tick.tick() => {
                if let Some((sid, ctl, _, reported)) = display_ctl.as_mut()
                    && let Some(a) = ctl.applied().filter(|a| a.request_id != *reported) {
                        *reported = a.request_id;
                        if a.status < 0 {
                            warn!("[{remote}] session {sid}: display request #{} failed 0x{:08X}", a.request_id, a.status as u32);
                            let _ = ctrl_tx.send(Message::Event(EventMsg { event: Event::Error as u32, value: a.status as u32, text: format!("display mode {}x{} not applied (0x{:08X})", a.mode.width, a.mode.height, a.status as u32) }));
                        } else {
                            info!("[{remote}] session {sid}: display {}x{} scale {}% applied (request #{})", a.mode.width, a.mode.height, a.mode.scale_percent, a.request_id);
                        }
                    }
                if let (Some(sid), path) = route.path() {
                    let settling = route.provider.is_some() && path != protocol::input_path::DEVICES && session_changed.elapsed() < Duration::from_secs(3);
                    if input_state != Some((sid, path)) && !settling {
                        input_state = Some((sid, path));
                        let text = match path {
                            protocol::input_path::DEVICES => "session keyboard/mouse devices".to_string(),
                            protocol::input_path::AGENT => "session agent (SendInput)".to_string(),
                            _ => format!("no input path for session {sid}"),
                        };
                        info!("[{remote}] input for session {sid}: {text}");
                        let _ = ctrl_tx.send(Message::Event(EventMsg { event: Event::InputStatus as u32, value: path, text }));
                    }
                }
            }
            _ = &mut client_reader => { info!("[{remote}] client control stream closed"); break; }
            e = conn.closed() => { info!("[{remote}] connection closed: {e}"); break; }
            _ = &mut provider_done => { info!("[{remote}] provider ended the connection"); break; }
        }
    }
    stop.store(true, Ordering::SeqCst);
    input_task.abort();
    route.send(protocol::InputEvent::ReleaseAll);
    drop(route);
    drop(provider_done); // drops the pipe write half
    if let Some(a) = provider_abort {
        a.abort(); // drops the read half -> pipe closed -> provider BrokenConnection -> session Disc
    }
    if let Some((s, h)) = video.take() {
        s.store(true, Ordering::SeqCst);
        let _ = h.await;
    }
    drop(ctrl_tx);
    let _ = tokio::time::timeout(Duration::from_secs(1), writer).await;
    conn.close(0u32.into(), b"bye");
    Ok(())
}

/// Writes the client's display mode into the session's control block (creating the frame section
/// if the driver has not yet, so the session starts at that mode). Deduplicated per session.
fn apply_display(
    remote: &std::net::SocketAddr,
    sid: u32,
    want: protocol::DisplayRequest,
    ctl: &mut Option<(u32, frame_shm::DisplayControl, Option<protocol::DisplayRequest>, u32)>,
) {
    let want = want.clamped(frame_shm::MAX_WIDTH, frame_shm::MAX_HEIGHT);
    if ctl.as_ref().map(|c| c.0) != Some(sid) {
        match frame_shm::DisplayControl::open_or_create(sid, true) {
            Ok(c) => *ctl = Some((sid, c, None, 0)),
            Err(e) => {
                warn!("[{remote}] session {sid}: display control unavailable: {e}");
                return;
            }
        }
    }
    let Some((_, c, last, _)) = ctl.as_mut() else { return };
    if *last == Some(want) {
        return;
    }
    let id = c.request(frame_shm::DisplayMode { width: want.width, height: want.height, refresh_hz: want.refresh_hz, scale_percent: want.scale_percent });
    *last = Some(want);
    info!("[{remote}] session {sid}: display request #{id} {}x{}@{} scale {}%", want.width, want.height, want.refresh_hz, want.scale_percent);
}
