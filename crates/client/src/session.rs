//! One remote session, independent of any UI: QUIC connection (TLS 1.3, pinned certificate),
//! control stream (HELLO/AUTH/events/feedback), video datagrams (FEC reassembly + OpenH264 decode)
//! and input (reliable uni stream for keys/buttons/wheel, datagrams for mouse moves).
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use protocol::asyncio::{read_message, write_message};
use protocol::{Auth, Event, EventMsg, Feedback, Hello, HelloAck, InputEvent, Message, Secret, StreamInfo, mouse_datagram};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;
use transport::Reassembler;

#[derive(Clone)]
pub struct SessionConfig {
    pub server: std::net::SocketAddr,
    /// SHA-256 of the trusted server certificate (hex); empty = nothing trusted yet. A mismatch
    /// aborts the TLS handshake (before credentials are sent) with [`UntrustedCert`].
    pub pin: String,
    pub hello: Hello,
    pub domain: String,
    pub user: String,
    pub password: Secret,
    pub record: Option<PathBuf>,
    /// initial remote display mode (sent right after AUTH)
    pub display: protocol::DisplayRequest,
    /// clipboard redirection (None = off)
    pub clipboard: Option<crate::clipboard::Backend>,
}

/// Decoded picture, tightly packed I420 planes (BT.709 limited range).
pub struct VideoFrame {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub frame_id: u32,
}

/// Newest decoded frame; the consumer is notified only when it has taken the previous one,
/// so a slow UI drops frames instead of queueing them.
#[derive(Default)]
pub struct FrameSlot {
    latest: Mutex<Option<Arc<VideoFrame>>>,
    pending: AtomicBool,
}

impl FrameSlot {
    /// Stores the frame; returns true if the consumer must be notified.
    fn put(&self, f: Arc<VideoFrame>) -> bool {
        *self.latest.lock().unwrap() = Some(f);
        !self.pending.swap(true, Ordering::AcqRel)
    }
    pub fn take(&self) -> Option<Arc<VideoFrame>> {
        self.pending.store(false, Ordering::Release);
        self.latest.lock().unwrap().clone()
    }
}

#[derive(Clone, Debug, Default)]
pub struct Stats {
    pub fps: f64,
    pub mbps: f64,
    pub datagrams_per_s: f64,
    pub lost: u64,
    pub recovered: u64,
    pub idr_requests: u64,
    pub decoded: u64,
    /// capture -> decoded latency (ms, p50/p95) - only when client and server share a clock (same host)
    pub latency_ms: Option<(f64, f64)>,
}

#[derive(Clone, Debug)]
pub enum Update {
    Connected {
        max_datagram: Option<usize>,
    },
    HelloAck(HelloAck),
    ServerEvent(EventMsg),
    Stream(StreamInfo),
    FrameReady,
    CursorShape(Arc<protocol::CursorShape>),
    CursorPos(protocol::CursorPos),
    /// clipboard activity (for the status line / headless log)
    Clipboard(String),
    Stats(Stats),
    /// handshake aborted: certificate not trusted (yet) - ask the user
    Untrusted(UntrustedCert),
    Ended(String),
}

#[derive(Clone, Copy, Debug)]
pub enum InputCmd {
    Event(InputEvent),
    Move {
        x: u16,
        y: u16,
    },
    /// window resized / DPI changed: ask the server for a new remote display mode
    Display(protocol::DisplayRequest),
    Disconnect,
}

/// The server presented a certificate that is not the trusted one (`expected` = None: first
/// connection). No credentials were sent.
#[derive(Debug, Clone)]
pub struct UntrustedCert {
    pub fingerprint: String,
    pub expected: Option<String>,
}

impl std::fmt::Display for UntrustedCert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.expected {
            None => write!(f, "server certificate is not trusted yet (SHA-256 {})", self.fingerprint),
            Some(e) => write!(f, "server certificate CHANGED: got SHA-256 {}, trusted {e}", self.fingerprint),
        }
    }
}
impl std::error::Error for UntrustedCert {}

/// "AB:CD:..." for display.
pub fn format_fingerprint(hex: &str) -> String {
    hex.as_bytes().chunks(2).map(|c| String::from_utf8_lossy(c).to_uppercase()).collect::<Vec<_>>().join(":")
}

#[derive(Debug)]
struct PinnedCert {
    sha256: String,
    seen: Arc<Mutex<Option<String>>>,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for PinnedCert {
    fn verify_server_cert(
        &self,
        end: &CertificateDer<'_>,
        _i: &[CertificateDer<'_>],
        _n: &ServerName<'_>,
        _o: &[u8],
        _t: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let got: String = Sha256::digest(end.as_ref()).iter().map(|b| format!("{b:02x}")).collect();
        *self.seen.lock().unwrap() = Some(got.clone());
        if !self.sha256.trim().is_empty() && got.eq_ignore_ascii_case(self.sha256.trim()) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(rustls::Error::General(format!("server certificate fingerprint mismatch: got {got}")))
        }
    }
    fn verify_tls12_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn verify_tls13_signature(&self, m: &[u8], c: &CertificateDer<'_>, d: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(m, c, d, &self.provider.signature_verification_algorithms)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

/// Microseconds on the same clock the server stamps frames with (Windows QPC). Elsewhere the
/// clocks are unrelated and latency is not reported.
fn capture_clock_us() -> Option<i64> {
    #[cfg(windows)]
    {
        Some(frame_shm::qpc_micros())
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn copy_plane(src: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h);
    for row in 0..h {
        v.extend_from_slice(&src[row * stride..row * stride + w]);
    }
    v
}

/// Splits a user name like RDP does: `DOMAIN\user` -> (DOMAIN, user); `user@upn.suffix` and plain
/// `user` are passed unchanged with an empty domain (Windows resolves UPNs / local accounts itself).
pub fn split_user(input: &str) -> (String, String) {
    let t = input.trim();
    match t.split_once('\\') {
        Some((d, u)) if !d.is_empty() && !u.is_empty() => (d.to_string(), u.to_string()),
        _ => (String::new(), t.to_string()),
    }
}

/// Executes clipboard protocol actions: PDUs to the session, remote content to the local clipboard.
async fn apply_clip(
    actions: Vec<crate::cliprdr::Action>,
    id: u32,
    conn: &quinn::Connection,
    out: &mut Option<quinn::SendStream>,
    set: Option<&std::sync::mpsc::Sender<crate::cliprdr::Content>>,
    emit: &mut (impl FnMut(Update) + Send),
) {
    for a in actions {
        match a {
            crate::cliprdr::Action::Send(pdu) => {
                if std::env::var_os("FASTRDP_DEBUG_CHANNELS").is_some() {
                    eprintln!("[cliprdr] -> {} bytes type {:?}", pdu.len(), pdu.get(..2));
                }
                if out.is_none() {
                    *out = conn.open_uni().await.ok();
                }
                if let Some(s) = out.as_mut() {
                    for f in protocol::ChannelData::fragments(id, &pdu) {
                        if write_message(s, &Message::ChannelData(f)).await.is_err() {
                            *out = None;
                            break;
                        }
                    }
                }
            }
            crate::cliprdr::Action::Info(m) => emit(Update::Clipboard(m)),
            crate::cliprdr::Action::SetLocal(c) => {
                emit(Update::Clipboard(format!("session clipboard copied here ({})", c.describe())));
                if let Some(s) = set {
                    let _ = s.send(c);
                }
            }
        }
    }
}

pub type BoxErr = Box<dyn std::error::Error + Send + Sync>;

/// Runs the session until the server closes it, the connection drops or `InputCmd::Disconnect`.
pub async fn run(
    cfg: SessionConfig,
    mut emit: impl FnMut(Update) + Send,
    frames: Arc<FrameSlot>,
    mut input: mpsc::UnboundedReceiver<InputCmd>,
) -> Result<Stats, BoxErr> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(Mutex::new(None::<String>));
    let mut tls = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedCert { sha256: cfg.pin.clone(), seen: seen.clone(), provider }))
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"fastrdp/1".to_vec()];
    let mut cc = quinn::ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(tls)?));
    let mut tc = quinn::TransportConfig::default();
    tc.max_idle_timeout(Some(Duration::from_secs(10).try_into()?));
    tc.keep_alive_interval(Some(Duration::from_secs(2)));
    tc.datagram_receive_buffer_size(Some(16 << 20));
    cc.transport_config(Arc::new(tc));
    let bind: std::net::SocketAddr = if cfg.server.is_ipv6() { "[::]:0".parse()? } else { "0.0.0.0:0".parse()? };
    let mut ep = quinn::Endpoint::client(bind)?;
    ep.set_default_client_config(cc);
    let conn = match ep.connect(cfg.server, "fastrdp")?.await {
        Ok(c) => c,
        Err(e) => {
            let pin = cfg.pin.trim();
            if let Some(fp) = seen.lock().unwrap().clone().filter(|fp| !fp.eq_ignore_ascii_case(pin)) {
                return Err(Box::new(UntrustedCert { fingerprint: fp, expected: (!pin.is_empty()).then(|| pin.to_ascii_lowercase()) }));
            }
            return Err(e.into());
        }
    };
    emit(Update::Connected { max_datagram: conn.max_datagram_size() });

    let (mut send, mut recv) = conn.open_bi().await?;
    write_message(&mut send, &Message::Hello(cfg.hello.clone())).await?;
    write_message(&mut send, &Message::Auth(Auth { domain: cfg.domain.clone(), user: cfg.user.clone(), password: cfg.password })).await?;
    write_message(&mut send, &Message::Display(cfg.display)).await?;
    // input stream (reliable, ordered): keys, buttons, wheel
    let mut input_send = conn.open_uni().await?;

    // virtual channels: server -> client on a server-opened uni stream, client -> server on our own
    let (chan_in_tx, mut chan_in) = mpsc::unbounded_channel::<Message>();
    {
        let conn = conn.clone();
        tokio::spawn(async move {
            while let Ok(mut s) = conn.accept_uni().await {
                let tx = chan_in_tx.clone();
                tokio::spawn(async move {
                    while let Ok(m) = read_message(&mut s).await {
                        if tx.send(m).is_err() {
                            break;
                        }
                    }
                });
            }
        });
    }
    let mut chan_out: Option<quinn::SendStream> = None;
    let mut chan_asm = protocol::ChannelAssembler::default();
    let mut clip_id: Option<u32> = None;
    let mut cliprdr = crate::cliprdr::Cliprdr::default();
    let (clip_changed_tx, mut clip_changed) = mpsc::unbounded_channel::<crate::cliprdr::Content>();
    let clip_set = cfg.clipboard.clone().map(|b| crate::clipboard::spawn(b, clip_changed_tx));

    let (ctl_tx, mut ctl_rx) = mpsc::unbounded_channel::<Message>();
    tokio::spawn(async move {
        while let Ok(m) = read_message(&mut recv).await {
            if ctl_tx.send(m).is_err() {
                break;
            }
        }
    });

    let mut reasm = Reassembler::new(Duration::from_millis(150));
    let mut dec = openh264::decoder::Decoder::new()?;
    let mut rec = cfg.record.as_ref().map(std::fs::File::create).transpose()?;
    let mut waiting_key = true;
    // the first decode error of each stream goes to the log (a stream the decoder rejects entirely
    // would otherwise only show as a black picture)
    let mut decode_error_logged = false;
    let mut last_frame_id: Option<u32> = None;
    let mut last_idr_req = Instant::now() - Duration::from_secs(1);
    let mut mouse_seq = 0u32;
    let mut stats = Stats::default();
    let (mut win_frames, mut win_bytes, mut win_dg) = (0u64, 0u64, 0u64);
    let mut lat: Vec<i64> = Vec::new();
    let mut win = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(50));
    let end_reason: String;
    let mut error_info = 0u32;
    loop {
        tokio::select! {
            m = ctl_rx.recv() => match m {
                Some(Message::HelloAck(a)) => {
                    if a.version != protocol::VERSION {
                        end_reason = format!(
                            "the server speaks FastRDP protocol {}, this client {}: update {}",
                            a.version,
                            protocol::VERSION,
                            if a.version > protocol::VERSION { "this client" } else { "the server" }
                        );
                        break;
                    }
                    emit(Update::HelloAck(a));
                }
                Some(Message::CursorShape(c)) => emit(Update::CursorShape(Arc::new(c))),
                Some(Message::CursorPos(c)) => emit(Update::CursorPos(c)),
                Some(Message::Event(e)) => {
                    let ev = Event::from_u32(e.event);
                    emit(Update::ServerEvent(e.clone()));
                    if ev == Some(Event::SetErrorInfo) && e.value != 0 {
                        error_info = e.value;
                    }
                    if matches!(ev, Some(Event::Closed) | Some(Event::Rejected) | Some(Event::LogOnlyMode)) {
                        end_reason = match protocol::disconnect_reason(error_info) {
                            Some(r) if ev == Some(Event::Closed) => r.to_string(),
                            _ if ev == Some(Event::Rejected) => format!("sign-in failed: {}", e.text),
                            _ => format!("server: {:?} {}", ev.unwrap(), e.text),
                        };
                        break;
                    }
                }
                Some(Message::StreamInfo(s)) => {
                    // new encoder instance (start, session switch, fallback): references are invalid
                    dec = openh264::decoder::Decoder::new()?;
                    waiting_key = true;
                    decode_error_logged = false;
                    emit(Update::Stream(s));
                }
                Some(_) => {}
                None => { end_reason = "control stream closed".into(); break; }
            },
            m = chan_in.recv() => match m {
                Some(Message::ChannelOpen(c)) => {
                    if c.name == crate::cliprdr::CHANNEL && clip_set.is_some() {
                        clip_id = Some(c.id);
                        cliprdr.reset();
                        emit(Update::Clipboard("clipboard channel open".into()));
                    }
                }
                Some(Message::ChannelData(d)) => {
                    if let Some((id, msg)) = chan_asm.push(d)
                        && Some(id) == clip_id {
                            if std::env::var_os("FASTRDP_DEBUG_CHANNELS").is_some() {
                                eprintln!("[cliprdr] <- {} bytes type {:?}", msg.len(), msg.get(..2));
                            }
                            let actions = cliprdr.on_pdu(&msg);
                            apply_clip(actions, id, &conn, &mut chan_out, clip_set.as_ref(), &mut emit).await;
                        }
                }
                Some(Message::ChannelClose(id)) => {
                    chan_asm.close(id);
                    if Some(id) == clip_id {
                        clip_id = None;
                    }
                }
                _ => {}
            },
            c = clip_changed.recv(), if clip_set.is_some() => {
                if let Some(c) = c {
                    let what = c.describe();
                    let actions = cliprdr.local_changed(c);
                    if let (Some(id), false) = (clip_id, actions.is_empty()) {
                        emit(Update::Clipboard(format!("local clipboard offered to the session ({what})")));
                        apply_clip(actions, id, &conn, &mut chan_out, clip_set.as_ref(), &mut emit).await;
                    }
                }
            }
            cmd = input.recv() => match cmd {
                Some(InputCmd::Event(e)) => {
                    if write_message(&mut input_send, &Message::Input(e)).await.is_err() {
                        end_reason = "input stream closed".into();
                        break;
                    }
                }
                Some(InputCmd::Move { x, y }) => {
                    mouse_seq = mouse_seq.wrapping_add(1);
                    let _ = conn.send_datagram(bytes::Bytes::copy_from_slice(&mouse_datagram::encode(mouse_seq, x, y)));
                }
                Some(InputCmd::Display(d)) => {
                    let _ = write_message(&mut send, &Message::Display(d)).await;
                }
                Some(InputCmd::Disconnect) | None => { end_reason = "disconnected by user".into(); break; }
            },
            d = conn.read_datagram() => {
                let d = match d { Ok(d) => d, Err(e) => { end_reason = format!("connection: {e}"); break; } };
                win_dg += 1;
                let (done, lost) = reasm.push(&d, Instant::now());
                if !lost.is_empty() {
                    stats.lost += lost.len() as u64;
                    waiting_key = true;
                    last_frame_id = lost.iter().copied().max().or(last_frame_id);
                }
                for f in done {
                    if let Some(prev) = last_frame_id {
                        let gap = f.frame_id.wrapping_sub(prev).wrapping_sub(1);
                        if gap > 0 && gap < 1_000_000 {
                            stats.lost += gap as u64;
                            waiting_key = true;
                        }
                    }
                    last_frame_id = Some(f.frame_id);
                    win_bytes += f.data.len() as u64;
                    stats.recovered += f.recovered as u64;
                    if waiting_key && !f.keyframe { continue; }
                    if f.keyframe { waiting_key = false; }
                    if let Some(r) = rec.as_mut() { use std::io::Write; r.write_all(&f.data)?; }
                    match dec.decode(&f.data) {
                        Ok(Some(y)) => {
                            use openh264::formats::YUVSource;
                            win_frames += 1;
                            stats.decoded += 1;
                            if let Some(now) = capture_clock_us() {
                                let l = now - f.pts_us;
                                if (0..5_000_000).contains(&l) { lat.push(l); }
                            }
                            let (w, h) = y.dimensions();
                            let (sy, su, sv) = y.strides();
                            let vf = VideoFrame {
                                width: w,
                                height: h,
                                y: copy_plane(y.y(), sy, w, h),
                                u: copy_plane(y.u(), su, w.div_ceil(2), h.div_ceil(2)),
                                v: copy_plane(y.v(), sv, w.div_ceil(2), h.div_ceil(2)),
                                frame_id: f.frame_id,
                            };
                            if frames.put(Arc::new(vf)) { emit(Update::FrameReady); }
                        }
                        Ok(None) => {}
                        Err(e) => {
                            if !decode_error_logged {
                                decode_error_logged = true;
                                crate::log::line(&format!("decode error (frame {}, {} bytes): {e}", f.frame_id, f.data.len()));
                            }
                            waiting_key = true;
                        }
                    }
                }
            },
            _ = tick.tick() => {
                let lost = reasm.poll(Instant::now());
                if !lost.is_empty() { stats.lost += lost.len() as u64; waiting_key = true; last_frame_id = lost.iter().copied().max().or(last_frame_id); }
                if waiting_key && last_idr_req.elapsed() > Duration::from_millis(200) && reasm.stats.frames_complete + reasm.stats.frames_lost > 0 {
                    last_idr_req = Instant::now();
                    stats.idr_requests += 1;
                    let f = Feedback { frames_complete: reasm.stats.frames_complete as u32, frames_lost: reasm.stats.frames_lost as u32, fec_recovered: reasm.stats.frames_recovered as u32, datagrams_received: reasm.stats.datagrams as u32, last_frame_id: 0, request_idr: 1 };
                    let _ = write_message(&mut send, &Message::Feedback(f)).await;
                }
                if win.elapsed() >= Duration::from_secs(1) {
                    let s = win.elapsed().as_secs_f64();
                    lat.sort_unstable();
                    let p = |q: f64| lat[((lat.len() - 1) as f64 * q) as usize] as f64 / 1000.0;
                    stats.fps = win_frames as f64 / s;
                    stats.mbps = win_bytes as f64 * 8.0 / s / 1e6;
                    stats.datagrams_per_s = win_dg as f64 / s;
                    stats.latency_ms = (!lat.is_empty()).then(|| (p(0.5), p(0.95)));
                    emit(Update::Stats(stats.clone()));
                    let f = Feedback { frames_complete: reasm.stats.frames_complete as u32, frames_lost: reasm.stats.frames_lost as u32, fec_recovered: reasm.stats.frames_recovered as u32, datagrams_received: reasm.stats.datagrams as u32, last_frame_id: 0, request_idr: 0 };
                    let _ = write_message(&mut send, &Message::Feedback(f)).await;
                    (win_frames, win_bytes, win_dg) = (0, 0, 0);
                    lat.clear();
                    win = Instant::now();
                }
            }
        }
    }
    let _ = write_message(&mut input_send, &Message::Input(InputEvent::ReleaseAll)).await;
    let _ = write_message(&mut send, &Message::Bye).await;
    let _ = send.finish();
    tokio::time::sleep(Duration::from_millis(150)).await;
    conn.close(0u32.into(), b"bye");
    emit(Update::Ended(end_reason));
    ep.wait_idle().await;
    Ok(stats)
}

#[cfg(test)]
mod tests {
    #[test]
    fn split_user() {
        assert_eq!(super::split_user(r"CORP\alice"), ("CORP".into(), "alice".into()));
        assert_eq!(super::split_user(r".\alice"), (".".into(), "alice".into()));
        assert_eq!(super::split_user("bob@example.com"), ("".into(), "bob@example.com".into()));
        assert_eq!(super::split_user(" alice "), ("".into(), "alice".into()));
    }
}
