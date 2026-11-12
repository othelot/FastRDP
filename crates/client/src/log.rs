//! GUI diagnostics: `<local data dir>/FastRDP/client.log` (Windows: %LOCALAPPDATA%). Panics and UI
//! errors are recorded there because a GUI process has no visible console. Never log secrets.
use std::io::Write;
use std::path::PathBuf;

pub fn path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Logs")
    } else {
        std::env::var_os("XDG_STATE_HOME").map(PathBuf::from).or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".local").join("state")))?
    };
    Some(base.join("FastRDP").join("client.log"))
}

pub fn line(msg: &str) {
    if cfg!(test) {
        return; // unit tests must not write into the user's log
    }
    let Some(p) = path() else { return };
    let _ = std::fs::create_dir_all(p.parent().unwrap());
    // keep the file small: start over above 1 MB
    if std::fs::metadata(&p).map(|m| m.len() > 1 << 20).unwrap_or(false) {
        let _ = std::fs::remove_file(&p);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
        let _ = writeln!(f, "{}.{:03} pid={} {msg}", t.as_secs(), t.subsec_millis(), std::process::id());
    }
}

/// Records panics (with backtrace) before the default handler runs.
pub fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        line(&format!("PANIC: {info}\n{}", std::backtrace::Backtrace::force_capture()));
        default(info);
    }));
}
