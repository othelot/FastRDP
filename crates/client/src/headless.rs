//! `--headless`: no window. Prints events and 1 s statistics, optionally records the stream.
//! Used for measurements.
use std::sync::Arc;
use std::time::{Duration, Instant};

use protocol::{Event, Secret};

use crate::session::{self, FrameSlot, InputCmd, SessionConfig, UntrustedCert, Update};

/// How the server certificate is trusted in headless mode.
pub struct Trust {
    /// host:port as given (key in the known-servers file)
    pub server_key: String,
    /// trust and remember an unknown (never a changed) certificate
    pub accept_new: bool,
}

pub fn run(mut cfg: SessionConfig, trust: Trust, secs: u64) -> i32 {
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("tokio");
    if cfg.pin.is_empty() {
        // handshake only (aborted by the verifier before any credential is sent) to learn the fingerprint
        let (_ptx, prx) = tokio::sync::mpsc::unbounded_channel();
        let mut probe = cfg.clone();
        probe.password = Secret::default();
        match rt.block_on(session::run(probe, |_| {}, Arc::new(FrameSlot::default()), prx)).err().and_then(|e| e.downcast::<UntrustedCert>().ok()) {
            Some(c) if trust.accept_new => {
                println!("trusting new server certificate SHA-256 {} (saved for {})", session::format_fingerprint(&c.fingerprint), trust.server_key);
                if let Err(e) = crate::known::save(&trust.server_key, &c.fingerprint) {
                    eprintln!("could not save it: {e}");
                }
                cfg.pin = c.fingerprint;
            }
            Some(c) => {
                eprintln!("server certificate is not trusted: SHA-256 {}", c.fingerprint);
                eprintln!("verify it on the server, then rerun with --pin {} (or --accept-new-cert to trust and remember it)", c.fingerprint);
                return 6;
            }
            None => {
                eprintln!("could not reach the server to read its certificate");
                return 1;
            }
        }
    }
    let start = Instant::now();
    let ts = move || format!("[{:7.3}s]", start.elapsed().as_secs_f64());
    let frames = Arc::new(FrameSlot::default());
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let stopper = tx.clone();
    rt.spawn(async move {
        tokio::time::sleep(Duration::from_secs(secs)).await;
        let _ = stopper.send(InputCmd::Disconnect);
    });
    let server = cfg.server;
    let emit = move |u: Update| match u {
        Update::Connected { max_datagram } => println!("{} QUIC connected to {server} (TLS 1.3, pinned certificate), max datagram {max_datagram:?}", ts()),
        Update::HelloAck(a) => println!("{} HELLO_ACK mode={} listener='{}' os='{}'", ts(), a.mode, a.listener, a.os_info),
        Update::ServerEvent(e) => println!("{} EVENT {:<22} value={} (0x{:X}) {}", ts(), format!("{:?}", Event::from_u32(e.event)), e.value, e.value, e.text),
        Update::Stream(s) => println!("{} STREAM {}x{}@{} encoder={} fallback={} ({})", ts(), s.width, s.height, s.fps, s.encoder, s.fallback, s.description),
        Update::Stats(s) => println!(
            "{} video {:5.1} fps {:6.2} Mbps {:5.0} dgram/s | lost {} recovered {} idr-req {} | capture->decoded {}",
            ts(),
            s.fps,
            s.mbps,
            s.datagrams_per_s,
            s.lost,
            s.recovered,
            s.idr_requests,
            s.latency_ms.map(|(a, b)| format!("p50 {a:.1} ms p95 {b:.1} ms")).unwrap_or_else(|| "n/a (server on another machine)".into()),
        ),
        Update::Ended(r) => println!("{} session ended: {r}", ts()),
        Update::Untrusted(_) => {}
        Update::CursorShape(c) => println!("{} CURSOR shape #{} {}x{} hotspot {},{}", ts(), c.id, c.width, c.height, c.hot_x, c.hot_y),
        Update::CursorPos(_) => {}
        Update::Clipboard(what) => println!("{} CLIPBOARD {what}", ts()),
        Update::FrameReady => {}
    };
    let res = rt.block_on(session::run(cfg, emit, frames, rx));
    drop(tx);

    match res {
        Ok(s) => {
            println!("TOTAL decoded {} frames, lost {}, fec-recovered {}, idr requests {}", s.decoded, s.lost, s.recovered, s.idr_requests);
            if s.decoded == 0 { 5 } else { 0 }
        }
        Err(e) => match e.downcast::<UntrustedCert>() {
            Ok(c) => {
                eprintln!("REFUSED: {c}");
                eprintln!("if the server key was regenerated on purpose, rerun with --pin {}", c.fingerprint);
                6
            }
            Err(e) => {
                eprintln!("session failed: {e}");
                1
            }
        },
    }
}
