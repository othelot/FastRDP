//! Minimal logger: %ProgramData%\FastRDP\logs\server.log (+ stderr in console mode). Never log secrets.
use std::io::Write;
use std::sync::Mutex;

static LOCK: Mutex<()> = Mutex::new(());

pub fn path() -> std::path::PathBuf {
    let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
    std::path::Path::new(&pd).join("FastRDP").join("logs").join("server.log")
}

pub fn write(level: &str, msg: &str) {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let line = format!("{}.{:03} pid={} {level} {msg}\n", now.as_secs(), now.subsec_millis(), std::process::id());
    let _g = LOCK.lock();
    eprint!("{line}");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path()) {
        let _ = f.write_all(line.as_bytes());
    }
}

#[macro_export]
macro_rules! info { ($($t:tt)*) => { $crate::log::write("INFO ", &format!($($t)*)) } }
#[macro_export]
macro_rules! warn { ($($t:tt)*) => { $crate::log::write("WARN ", &format!($($t)*)) } }
