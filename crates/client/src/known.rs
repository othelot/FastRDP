//! Trusted server certificates ("known servers"), like RDP's "don't ask me again" / ssh known_hosts.
//! File: `<config dir>/FastRDP/known_servers`, one `<host:port> <sha256-hex>` per line.
//! Config dir: %APPDATA% (Windows), ~/Library/Application Support (macOS), $XDG_CONFIG_HOME or ~/.config.
use std::path::PathBuf;

pub fn path() -> Option<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA")?)
    } else if cfg!(target_os = "macos") {
        PathBuf::from(std::env::var_os("HOME")?).join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).or_else(|| Some(PathBuf::from(std::env::var_os("HOME")?).join(".config")))?
    };
    Some(base.join("FastRDP").join("known_servers"))
}

fn key(server: &str) -> String {
    server.trim().to_ascii_lowercase()
}

fn entries() -> Vec<(String, String)> {
    let Some(p) = path() else { return Vec::new() };
    std::fs::read_to_string(p)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            Some((it.next()?.to_string(), it.next()?.to_ascii_lowercase()))
        })
        .collect()
}

/// Trusted fingerprint for `server` (as typed, host:port).
pub fn get(server: &str) -> Option<String> {
    let k = key(server);
    entries().into_iter().find(|(s, _)| *s == k).map(|(_, f)| f)
}

/// Remembers (or replaces) the fingerprint for `server`.
pub fn save(server: &str, fingerprint: &str) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let k = key(server);
    let mut list: Vec<_> = entries().into_iter().filter(|(s, _)| *s != k).collect();
    list.push((k, fingerprint.to_ascii_lowercase()));
    std::fs::create_dir_all(p.parent().unwrap())?;
    let body: String = list.iter().map(|(s, f)| format!("{s} {f}\n")).collect();
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, p)
}
