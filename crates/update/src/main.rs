//! fastrdp-update - keeps the FastRDP server components up to date (Windows, run as SYSTEM by the
//! "FastRDP Update" scheduled task).
//!
//! `apply` fetches the signed manifest of the latest release; if it is newer than this build and no
//! remote session is active (installing restarts the Remote Desktop service, which would drop it),
//! it downloads and verifies the server package, extracts it under `<install dir>\packages` (only
//! Administrators can write there) and runs the package's `Install-FastRdp.ps1`. If that fails, the
//! previously installed package is installed again. Log: `%ProgramData%\FastRDP\logs\update.log`.
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "fastrdp-update", version, about = "FastRDP updater (server components)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the installed and the latest version (exit code 10 = an update is available)
    Check,
    /// Install the latest version if it is newer
    Apply {
        /// Install even while remote sessions are active (they will be disconnected)
        #[arg(long)]
        now: bool,
        /// Started by the scheduled task: do nothing when automatic updates are turned off
        #[arg(long)]
        scheduled: bool,
    },
}

#[cfg(not(windows))]
fn main() {
    let _ = Cli::parse();
    eprintln!("fastrdp-update runs on Windows only");
    std::process::exit(2);
}

#[cfg(windows)]
fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Check => win::check(),
        Cmd::Apply { now, scheduled } => win::apply(now, scheduled),
    };
    std::process::exit(code);
}

#[cfg(windows)]
mod win {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use update::{Release, Version};
    use windows_sys::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    use windows_sys::Win32::System::RemoteDesktop::{WTS_SESSION_INFOW, WTSActive, WTSEnumerateSessionsW, WTSFreeMemory};

    const SETTINGS_KEY: &str = r"SOFTWARE\FastRDP";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn log(msg: &str) {
        println!("{msg}");
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        let dir = Path::new(&pd).join("FastRDP").join("logs");
        let _ = std::fs::create_dir_all(&dir);
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("update.log")) {
            let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
            let _ = writeln!(f, "{ts:.3} pid={} {msg}", std::process::id());
        }
    }

    fn reg_dword(value: &str) -> Option<u32> {
        let (k, v) = (wide(SETTINGS_KEY), wide(value));
        let mut data = 0u32;
        let mut size = 4u32;
        // SAFETY: sized out buffer.
        let r = unsafe {
            RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_DWORD, std::ptr::null_mut(), (&mut data as *mut u32).cast(), &mut size)
        };
        (r == 0).then_some(data)
    }

    fn reg_string(value: &str) -> Option<String> {
        let (k, v) = (wide(SETTINGS_KEY), wide(value));
        let mut buf = [0u16; 128];
        let mut size = (buf.len() * 2) as u32;
        // SAFETY: sized out buffer.
        let r = unsafe { RegGetValueW(HKEY_LOCAL_MACHINE, k.as_ptr(), v.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut size) };
        if r != 0 {
            return None;
        }
        let n = (size as usize / 2).saturating_sub(1);
        Some(String::from_utf16_lossy(&buf[..n]))
    }

    /// Names of active sessions other than the console (RDP-Tcp#n, FastRDP#n, ...).
    fn active_remote_sessions() -> Vec<String> {
        let mut out = Vec::new();
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        // SAFETY: WTS_CURRENT_SERVER_HANDLE (null); the array is freed with WTSFreeMemory.
        unsafe {
            if WTSEnumerateSessionsW(std::ptr::null_mut(), 0, 1, &mut info, &mut count) == 0 {
                return vec!["(session list unavailable)".into()];
            }
            for s in std::slice::from_raw_parts(info, count as usize) {
                if s.State != WTSActive || s.pWinStationName.is_null() {
                    continue;
                }
                let mut n = 0;
                while *s.pWinStationName.add(n) != 0 {
                    n += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(s.pWinStationName, n));
                if !name.eq_ignore_ascii_case("Console") {
                    out.push(format!("{name} (session {})", s.SessionId));
                }
            }
            WTSFreeMemory(info.cast());
        }
        out
    }

    fn install_dir() -> PathBuf {
        std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).unwrap_or_else(|| PathBuf::from(r"C:\Program Files\FastRDP"))
    }

    pub fn check() -> i32 {
        println!("installed: {}", update::CURRENT);
        match update::latest() {
            Ok(r) if r.version > Version::current() => {
                println!("available: {}", r.version);
                10
            }
            Ok(r) => {
                println!("latest:    {} (up to date)", r.version);
                0
            }
            Err(e) => {
                eprintln!("{e}");
                1
            }
        }
    }

    /// Runs `<package>\scripts\Install-FastRdp.ps1 -PackageDir <package> -Unattended`.
    fn run_install(package: &Path) -> bool {
        let script = package.join("scripts").join("Install-FastRdp.ps1");
        let windir = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        let ps = Path::new(&windir).join(r"System32\WindowsPowerShell\v1.0\powershell.exe");
        log(&format!("running {}", script.display()));
        let out = Command::new(ps)
            .args(["-NoProfile", "-NonInteractive", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .arg("-PackageDir")
            .arg(package)
            .arg("-Unattended")
            .output();
        match out {
            Ok(o) => {
                for line in String::from_utf8_lossy(&o.stdout).lines().chain(String::from_utf8_lossy(&o.stderr).lines()) {
                    if !line.trim().is_empty() {
                        log(&format!("  | {line}"));
                    }
                }
                log(&format!("install script exit code {:?}", o.status.code()));
                o.status.success()
            }
            Err(e) => {
                log(&format!("cannot start PowerShell: {e}"));
                false
            }
        }
    }

    fn fetch(release: &Release, packages: &Path) -> Result<PathBuf, update::Error> {
        let staging = packages.join(".download");
        let _ = std::fs::remove_dir_all(&staging);
        let zip = update::download(release, &release.server_package(), &staging)?;
        log(&format!("downloaded and verified {}", zip.display()));
        let name = release.server_package().trim_end_matches(".zip").to_string();
        let dest = packages.join(&name);
        let _ = std::fs::remove_dir_all(&dest);
        update::extract(&zip, packages)?;
        let _ = std::fs::remove_dir_all(&staging);
        if !dest.join("scripts").join("Install-FastRdp.ps1").is_file() {
            return Err(update::Error::Integrity(format!("{name}: no scripts\\Install-FastRdp.ps1 in the package")));
        }
        Ok(dest)
    }

    pub fn apply(now: bool, scheduled: bool) -> i32 {
        if scheduled && reg_dword("AutoUpdate") == Some(0) {
            println!("automatic updates are turned off");
            return 0;
        }
        let release = match update::newer() {
            Ok(Some(r)) => r,
            Ok(None) => {
                if !scheduled {
                    println!("FastRDP {} is up to date", update::CURRENT);
                }
                return 0;
            }
            Err(e) => {
                log(&format!("update check failed: {e}"));
                return 1;
            }
        };
        log(&format!("FastRDP {} is available (installed {})", release.version, update::CURRENT));
        let sessions = active_remote_sessions();
        if !now && !sessions.is_empty() {
            log(&format!("deferred: remote session(s) active: {}", sessions.join(", ")));
            return 0;
        }
        let packages = install_dir().join("packages");
        let package = match fetch(&release, &packages) {
            Ok(p) => p,
            Err(e) => {
                log(&format!("download failed: {e}"));
                return 1;
            }
        };
        let previous = reg_string("InstalledVersion");
        if run_install(&package) {
            log(&format!("updated to {}", release.version));
            return 0;
        }
        log(&format!("update to {} failed", release.version));
        match previous.map(|v| packages.join(format!("fastrdp-{v}-x64"))) {
            Some(prev) if prev.join("scripts").join("Install-FastRdp.ps1").is_file() && prev != package => {
                log(&format!("rolling back to {}", prev.display()));
                if run_install(&prev) {
                    log("rollback done");
                } else {
                    log("ROLLBACK FAILED - reinstall with: irm https://fastrdp.dev | iex");
                }
            }
            _ => log("no previous package to roll back to"),
        }
        1
    }
}
