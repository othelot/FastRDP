//! fastrdp-client - FastRDP client: iced GUI, or `--headless` for measurements.
//!
//! Server certificates: the GUI asks on first contact (fingerprint + "don't ask again", like RDP)
//! and warns loudly if a remembered certificate changes. Headless refuses unknown certificates
//! unless `--accept-new-cert` (trust on first use) or `--pin`/`--pin-file`. The password is never
//! printed; pass it through an environment variable (`--password-env`) or the prompt.
//!
//! Windows: a GUI-subsystem program (no console window next to the GUI). Started from a shell,
//! it attaches to that console, so `--headless`, `--help` and errors still print there.
#![cfg_attr(windows, windows_subsystem = "windows")]
mod clipboard;
mod cliprdr;
mod connbar;
mod display;
mod headless;
#[cfg(windows)]
mod keyhook;
mod keymap;
mod known;
mod log;
#[cfg(windows)]
mod maximize;
#[cfg(windows)]
mod native_cursor;
mod prefs;
mod selfupdate;
mod session;
mod ui;
mod video;

use std::path::PathBuf;

use clap::Parser;
use protocol::{Hello, Secret};

#[derive(Parser, Debug)]
#[command(name = "fastrdp-client", version, about = "FastRDP client (GUI; --headless for automation)")]
struct Cli {
    /// Server address (host:port, default port 33890)
    #[arg(long, value_name = "HOST:PORT")]
    server: Option<String>,
    /// User name: `name`, `DOMAIN\name` or `name@domain` (like the RDP client)
    #[arg(long)]
    user: Option<String>,
    /// Read the password from this environment variable (GUI: connect immediately)
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,
    /// Trust exactly this server certificate (SHA-256 hex) instead of the known-servers file
    #[arg(long, value_name = "SHA256", conflicts_with = "pin_file")]
    pin: Option<String>,
    /// Like --pin, read from a file (e.g. the server's cert.sha256)
    #[arg(long, value_name = "FILE")]
    pin_file: Option<PathBuf>,
    /// Remote desktop size, e.g. 2560x1440 (headless; GUI: preselects the slider)
    #[arg(long, value_name = "WxH", value_parser = parse_resolution)]
    resolution: Option<(u32, u32)>,
    /// Remote DPI scale in percent (headless; the GUI uses the window's scale)
    #[arg(long, default_value_t = 100)]
    scale: u32,
    /// Session experience preset: quality (all effects), balanced, performance (no effects).
    /// Session-only: the remote user's settings are not changed. GUI: overrides the saved choice
    #[arg(long, value_parser = ["quality", "balanced", "performance"])]
    experience: Option<String>,
    /// Session experience as raw WTS_PERF_* flags (hex, e.g. 0x180); overrides --experience
    #[arg(long, value_name = "HEX", value_parser = parse_hex)]
    perf_flags: Option<u32>,
    /// GUI: synchronise presentation with the display refresh (default off = lowest latency;
    /// ICED_PRESENT_MODE overrides)
    #[arg(long)]
    vsync: bool,

    /// Run without a window: print events/stats, optional recording
    #[arg(long)]
    headless: bool,
    /// Headless: trust and remember an unknown server certificate (never a changed one)
    #[arg(long, requires = "headless")]
    accept_new_cert: bool,
    /// Headless: disconnect after N seconds
    #[arg(long, default_value_t = 3600, requires = "headless")]
    secs: u64,
    /// Headless: write the received H.264 elementary stream to this file
    #[arg(long, value_name = "FILE", requires = "headless")]
    record: Option<PathBuf>,
    /// Do not share the clipboard with the session
    #[arg(long)]
    no_clipboard: bool,
    /// GUI: do not look for a newer FastRDP release at start
    #[arg(long)]
    no_update_check: bool,
}

fn parse_resolution(s: &str) -> Result<(u32, u32), String> {
    let (w, h) = s.split_once(['x', 'X', '*']).ok_or("expected WIDTHxHEIGHT, e.g. 1920x1080")?;
    Ok((w.trim().parse().map_err(|_| "bad width")?, h.trim().parse().map_err(|_| "bad height")?))
}

fn parse_hex(s: &str) -> Result<u32, String> {
    u32::from_str_radix(s.trim_start_matches("0x").trim_start_matches("0X"), 16).map_err(|e| e.to_string())
}

impl Cli {
    fn performance_flags(&self) -> Option<u32> {
        use protocol::perf;
        self.perf_flags.map(|f| f & perf::VALID).or(match self.experience.as_deref() {
            Some("quality") => Some(perf::BEST_QUALITY),
            Some("balanced") => Some(perf::BALANCED),
            Some("performance") => Some(perf::BEST_PERFORMANCE),
            _ => None,
        })
    }
}

fn resolve(server: &str) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    let s = server.trim();
    let with_port = if s.rsplit_once(':').is_some_and(|(_, p)| p.parse::<u16>().is_ok()) { s.to_string() } else { format!("{s}:33890") };
    with_port.to_socket_addrs().ok()?.next()
}

/// Windows GUI-subsystem build: use the console of the parent shell, if any, for stdout/stderr.
fn attach_parent_console() {
    #[cfg(windows)]
    // SAFETY: plain Win32 call; fails harmlessly when there is no parent console.
    unsafe {
        windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS);
    }
}

fn main() {
    attach_parent_console();
    let cli = Cli::parse();
    let pin = match (&cli.pin, &cli.pin_file) {
        (Some(p), _) => Some(p.trim().to_string()),
        (None, Some(f)) => match std::fs::read_to_string(f) {
            Ok(s) => Some(s.trim().to_string()),
            Err(e) => {
                eprintln!("--pin-file {}: {e}", f.display());
                std::process::exit(2);
            }
        },
        _ => None,
    };
    let password_env = cli.password_env.as_ref().and_then(|v| std::env::var(v).ok());

    if cli.headless {
        let (Some(server_key), Some(user)) = (cli.server.clone(), cli.user.clone()) else {
            eprintln!("--headless requires --server and --user");
            std::process::exit(2);
        };
        let Some(server) = resolve(&server_key) else {
            eprintln!("cannot resolve {server_key}");
            std::process::exit(2);
        };
        let pin = pin.or_else(|| known::get(&server_key)).unwrap_or_default();
        let password = match password_env {
            Some(p) => Secret::from_str(&p),
            None => Secret::from_str(&rpassword::prompt_password(format!("Password for {user}: ")).unwrap_or_default()),
        };
        let (domain, user) = session::split_user(&user);
        let res = cli.resolution.unwrap_or((1920, 1080));
        let cfg = session::SessionConfig {
            server,
            pin,
            hello: Hello {
                width: res.0 as u16,
                height: res.1 as u16,
                refresh_hz: 60,
                client_name: ui::hostname(),
                performance_flags: cli
                    .performance_flags()
                    .or_else(|| prefs::get(&server_key).map(|p| p.performance_flags))
                    .unwrap_or(protocol::perf::BEST_QUALITY),
            },
            display: protocol::DisplayRequest { width: res.0, height: res.1, refresh_hz: 60, scale_percent: cli.scale }.clamped(display::MAX.0, display::MAX.1),
            domain,
            user,
            password,
            record: cli.record,
            clipboard: None, // headless: no clipboard
        };
        let trust = headless::Trust { server_key, accept_new: cli.accept_new_cert };
        std::process::exit(headless::run(cfg, trust, cli.secs));
    }

    let auto_connect = password_env.is_some() && cli.server.is_some() && cli.user.is_some();
    let saved = cli.server.as_deref().and_then(prefs::get).unwrap_or_default();
    let form = ui::Form {
        performance_flags: cli.performance_flags().unwrap_or(saved.performance_flags),
        display: saved.display,
        dynamic: saved.dynamic,
        clipboard: saved.clipboard && !cli.no_clipboard,
        keys: saved.keys,
        server: cli.server.unwrap_or_default(),
        pin: pin.unwrap_or_default(),
        user: cli.user.unwrap_or_default(),
        password: password_env.unwrap_or_default(),
    };
    log::install_panic_hook();
    log::line(&format!("GUI start {} (server {:?})", env!("CARGO_PKG_VERSION"), form.server));
    let r = ui::run(ui::Launch { form, auto_connect, vsync: cli.vsync, update_check: !cli.no_update_check });
    #[cfg(windows)]
    {
        let (sent, local, failed) = keyhook::stats();
        for t in keyhook::trace() {
            log::line(&format!("keyhook trace: {t}"));
        }
        log::line(&format!("GUI exit: key hook sent {sent} key(s) to the session, left {local} local, {failed} send failure(s)"));
    }
    if let Err(e) = r {
        log::line(&format!("ui error: {e}"));
        eprintln!("ui: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli_definition_is_valid() {
        super::Cli::command().debug_assert();
    }

    #[test]
    fn headless_only_options_require_headless() {
        assert!(super::Cli::try_parse_from(["c", "--server", "h:1", "--secs", "5"]).is_err());
        let c = super::Cli::try_parse_from(["c", "--headless", "--server", "h:1", "--user", "u", "--secs", "5", "--accept-new-cert"]).unwrap();
        assert!(c.headless && c.accept_new_cert && c.secs == 5);
        assert!(super::Cli::try_parse_from(["c", "--pin", "aa", "--pin-file", "f"]).is_err());
    }

    #[test]
    fn experience_arguments() {
        let c = super::Cli::try_parse_from(["c", "--experience", "performance"]).unwrap();
        assert_eq!(c.performance_flags(), Some(protocol::perf::BEST_PERFORMANCE));
        let c = super::Cli::try_parse_from(["c", "--experience", "balanced", "--perf-flags", "0x180"]).unwrap();
        assert_eq!(c.performance_flags(), Some(0x180));
        assert!(super::Cli::try_parse_from(["c", "--experience", "fast"]).is_err());
        assert_eq!(super::Cli::try_parse_from(["c"]).unwrap().performance_flags(), None);
    }

    #[test]
    fn resolution_argument() {
        assert_eq!(super::parse_resolution("2560x1440"), Ok((2560, 1440)));
        assert!(super::parse_resolution("2560").is_err());
    }

    #[test]
    fn default_port() {
        assert_eq!(super::resolve("127.0.0.1").unwrap().port(), 33890);
        assert_eq!(super::resolve("127.0.0.1:5000").unwrap().port(), 5000);
    }
}
