//! fastrdp-server - FastRDP network endpoint (QUIC/UDP) running as a LOCAL SERVICE Windows service.
//!
//! Console mode by default (Ctrl+C stops); `--service` is the SCM entry point used by the installer.
mod cert;
mod input;
mod log;
mod server;
mod video;

use std::ffi::OsString;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tokio::sync::watch;

const SERVICE_NAME: &str = "FastRdpServer";

#[derive(clap::Parser, Debug)]
#[command(name = "fastrdp-server", version, about = "FastRDP server: QUIC endpoint, provider relay, video encoding, input relay")]
struct Cli {
    /// UDP address to listen on
    #[arg(long, default_value = "0.0.0.0:33890")]
    bind: SocketAddr,
    /// Name of the FastRDP listener (WinStation) whose provider pipe to use
    #[arg(long, default_value = "FastRDP")]
    listener: String,
    /// Base forward-error-correction ratio (raised automatically with reported loss)
    #[arg(long, default_value_t = 0.15)]
    fec: f32,
    /// Data directory (certificate, logs); default %ProgramData%\FastRDP
    #[arg(long)]
    data_dir: Option<PathBuf>,
    /// Run as the Windows service (SCM entry point)
    #[arg(long)]
    service: bool,
}

impl Cli {
    fn config(&self) -> server::Config {
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        server::Config {
            bind: self.bind,
            listener: self.listener.clone(),
            fec_base: self.fec,
            data_dir: self.data_dir.clone().unwrap_or_else(|| PathBuf::from(pd).join("FastRDP")),
        }
    }
}

fn run_blocking(cli: &Cli, shutdown: watch::Receiver<bool>) -> std::io::Result<()> {
    let cfg = cli.config();
    info!("fastrdp-server {} starting: {cfg:?}", env!("CARGO_PKG_VERSION"));
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let r = rt.block_on(server::run(cfg, shutdown));
    rt.shutdown_timeout(Duration::from_secs(2));
    r
}

windows_service::define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<OsString>) {
    use windows_service::service::{ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus, ServiceType};
    use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
    let (tx, rx) = watch::channel(false);
    let handler = move |c| match c {
        ServiceControl::Stop | ServiceControl::Shutdown => {
            let _ = tx.send(true);
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    };
    let Ok(h) = service_control_handler::register(SERVICE_NAME, handler) else { return };
    let status = |state, accept| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: accept,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::from_secs(5),
        process_id: None,
    };
    let _ = h.set_service_status(status(ServiceState::Running, ServiceControlAccept::STOP | ServiceControlAccept::SHUTDOWN));
    // The SCM passes start arguments separately; use the image-path arguments.
    let cli = <Cli as clap::Parser>::parse();
    if let Err(e) = run_blocking(&cli, rx) {
        warn!("service run failed: {e}");
    }
    let _ = h.set_service_status(status(ServiceState::Stopped, ServiceControlAccept::empty()));
}

fn main() {
    let cli = <Cli as clap::Parser>::parse();
    if cli.service {
        if let Err(e) = windows_service::service_dispatcher::start(SERVICE_NAME, ffi_service_main) {
            warn!("service dispatcher: {e}");
        }
        return;
    }
    let (tx, rx) = watch::channel(false);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async {
            let _ = tokio::signal::ctrl_c().await;
        });
        let _ = tx.send(true);
    });
    if let Err(e) = run_blocking(&cli, rx) {
        warn!("run failed: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use clap::{CommandFactory, Parser};

    #[test]
    fn cli() {
        super::Cli::command().debug_assert();
        let c = super::Cli::try_parse_from(["s", "--service", "--bind", "0.0.0.0:33890", "--listener", "FastRDP"]).unwrap();
        assert!(c.service);
        assert_eq!(c.config().listener, "FastRDP");
    }
}
