//! Per-server connection preferences (no secrets): `<config dir>/FastRDP/connections`,
//! one line per server: `<host:port> perf=0x1EF res=<slider index or full> dynamic=0|1`.
use std::path::PathBuf;

/// Where Windows key combinations go (saved per server).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum KeyCombos {
    /// always to the session while the window is active (mstsc default)
    #[default]
    Remote,
    /// only in full screen
    FullScreen,
    /// never (the local system handles them)
    Local,
}

impl KeyCombos {
    pub const ALL: [KeyCombos; 3] = [KeyCombos::Remote, KeyCombos::FullScreen, KeyCombos::Local];
    pub fn key(self) -> &'static str {
        match self {
            KeyCombos::Remote => "remote",
            KeyCombos::FullScreen => "fullscreen",
            KeyCombos::Local => "local",
        }
    }
    pub fn from_key(s: &str) -> KeyCombos {
        KeyCombos::ALL.into_iter().find(|m| m.key() == s).unwrap_or_default()
    }
}

impl std::fmt::Display for KeyCombos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            KeyCombos::Remote => "On the remote computer",
            KeyCombos::FullScreen => "Only in full screen",
            KeyCombos::Local => "On this computer",
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Prefs {
    pub performance_flags: u32,
    /// resolution slider position; None = full screen
    pub display: Option<u16>,
    pub dynamic: bool,
    pub clipboard: bool,
    pub keys: KeyCombos,
    /// remote desktop in physical pixels at the display's scale (Retina / HiDPI); off = in
    /// logical points at 100%, enlarged locally
    pub hidpi: bool,
    /// video bitrate target in Mbit/s; 0 = automatic (the server's setting)
    pub bitrate_mbps: u32,
}

impl Default for Prefs {
    fn default() -> Self {
        Prefs {
            performance_flags: protocol::perf::BEST_QUALITY,
            display: None,
            dynamic: true,
            clipboard: true,
            keys: KeyCombos::Remote,
            hidpi: true,
            bitrate_mbps: 0,
        }
    }
}

fn path() -> Option<PathBuf> {
    Some(crate::known::path()?.with_file_name("connections"))
}

fn key(server: &str) -> String {
    server.trim().to_ascii_lowercase()
}

fn parse(line: &str) -> Option<(String, Prefs)> {
    let mut it = line.split_whitespace();
    let server = it.next()?.to_string();
    let mut p = Prefs::default();
    for kv in it {
        match kv.split_once('=') {
            Some(("perf", v)) => p.performance_flags = u32::from_str_radix(v.trim_start_matches("0x"), 16).ok()? & protocol::perf::VALID,
            Some(("res", "full")) => p.display = None,
            Some(("res", v)) => p.display = v.parse().ok(),
            Some(("dynamic", v)) => p.dynamic = v == "1",
            Some(("clipboard", v)) => p.clipboard = v == "1",
            Some(("keys", v)) => p.keys = KeyCombos::from_key(v),
            Some(("hidpi", v)) => p.hidpi = v == "1",
            Some(("bitrate", v)) => p.bitrate_mbps = v.parse().unwrap_or(0),
            _ => {}
        }
    }
    Some((server, p))
}

fn format(server: &str, p: &Prefs) -> String {
    let res = p.display.map(|i| i.to_string()).unwrap_or_else(|| "full".into());
    format!(
        "{server} perf=0x{:X} res={res} dynamic={} clipboard={} keys={} hidpi={} bitrate={}\n",
        p.performance_flags,
        p.dynamic as u8,
        p.clipboard as u8,
        p.keys.key(),
        p.hidpi as u8,
        p.bitrate_mbps
    )
}

fn entries() -> Vec<(String, Prefs)> {
    let Some(p) = path() else { return Vec::new() };
    std::fs::read_to_string(p).unwrap_or_default().lines().filter_map(parse).collect()
}

pub fn get(server: &str) -> Option<Prefs> {
    let k = key(server);
    entries().into_iter().find(|(s, _)| *s == k).map(|(_, p)| p)
}

pub fn save(server: &str, prefs: &Prefs) -> std::io::Result<()> {
    let p = path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    let k = key(server);
    if k.is_empty() {
        return Ok(());
    }
    let mut list: Vec<_> = entries().into_iter().filter(|(s, _)| *s != k).collect();
    list.push((k, *prefs));
    std::fs::create_dir_all(p.parent().unwrap())?;
    let body: String = list.iter().map(|(s, p)| format(s, p)).collect();
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(tmp, p)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_roundtrip() {
        let p = Prefs {
            performance_flags: protocol::perf::BALANCED,
            display: Some(7),
            dynamic: false,
            clipboard: false,
            keys: KeyCombos::FullScreen,
            hidpi: false,
            bitrate_mbps: 40,
        };
        let line = format("pc:33890", &p);
        assert_eq!(parse(line.trim()), Some(("pc:33890".to_string(), p)));
        let full = Prefs { display: None, ..Prefs::default() };
        assert_eq!(parse(format("x:1", &full).trim()).unwrap().1, full);
        assert_eq!(parse("pc:1 perf=0xFFFF").unwrap().1.performance_flags, protocol::perf::VALID);
        assert_eq!(KeyCombos::from_key("nonsense"), KeyCombos::Remote);
        assert!(parse("pc:1 perf=0x180").unwrap().1.hidpi, "lines written before the option: HiDPI on");
    }
}
