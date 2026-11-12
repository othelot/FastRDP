//! User-configurable encoder settings (`%ProgramData%\FastRDP\encoder.toml`).
//!
//! ```toml
//! encoder = "x264"          # x264 (default) | openh264 | nvenc, mf (not implemented yet -> x264)
//! bitrate_kbps = 15000     # at 1920x1080@60; scaled to the session's size and refresh rate,
//!                           # unless the client sets its own target
//! fps = 60                  # the streaming server uses the client's refresh rate instead
//! keyint_secs = 0           # 0 = no periodic IDR (IDR only on request / fallback)
//! retry_requested_secs = 30 # after a fallback, retry the configured encoder every N s (0 = never)
//!
//! [x264]
//! preset = "superfast"
//! tune = "zerolatency"
//! profile = "high"
//! threads = 0               # 0 = auto
//! intra_refresh = false
//! vbv_ms = 200
//!
//! [openh264]
//! usage = "screen"          # screen | camera
//! threads = 0
//! ```
//! Whatever is configured, **x264 is the fallback** when that encoder is missing or fails.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EncoderKind {
    #[default]
    X264,
    OpenH264,
    Nvenc,
    #[serde(rename = "mf")]
    MediaFoundation,
}

impl fmt::Display for EncoderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            EncoderKind::X264 => "x264",
            EncoderKind::OpenH264 => "openh264",
            EncoderKind::Nvenc => "nvenc",
            EncoderKind::MediaFoundation => "mf",
        })
    }
}

impl FromStr for EncoderKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        match s.to_ascii_lowercase().as_str() {
            "x264" => Ok(EncoderKind::X264),
            "openh264" => Ok(EncoderKind::OpenH264),
            "nvenc" => Ok(EncoderKind::Nvenc),
            "mf" | "mediafoundation" => Ok(EncoderKind::MediaFoundation),
            o => Err(format!("unknown encoder '{o}' (x264|openh264|nvenc|mf)")),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct X264Settings {
    pub preset: String,
    pub tune: String,
    pub profile: String,
    pub threads: u32,
    pub intra_refresh: bool,
    pub vbv_ms: u32,
}

impl Default for X264Settings {
    fn default() -> Self {
        X264Settings { preset: "superfast".into(), tune: "zerolatency".into(), profile: "high".into(), threads: 0, intra_refresh: false, vbv_ms: 200 }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OpenH264Settings {
    pub usage: String,
    pub threads: u16,
}

impl Default for OpenH264Settings {
    fn default() -> Self {
        OpenH264Settings { usage: "screen".into(), threads: 0 }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct EncoderSettings {
    pub encoder: EncoderKind,
    pub bitrate_kbps: u32,
    pub fps: u32,
    pub keyint_secs: u32,
    pub retry_requested_secs: u32,
    pub x264: X264Settings,
    pub openh264: OpenH264Settings,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        EncoderSettings {
            encoder: EncoderKind::X264,
            bitrate_kbps: 15_000,
            fps: 60,
            keyint_secs: 0,
            retry_requested_secs: 30,
            x264: X264Settings::default(),
            openh264: OpenH264Settings::default(),
        }
    }
}

impl EncoderSettings {
    pub fn default_path() -> PathBuf {
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        Path::new(&pd).join("FastRDP").join("encoder.toml")
    }

    /// Missing file -> defaults (x264). Malformed file -> error (never silently ignore user config).
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        }
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        let s: EncoderSettings = toml::from_str(text).map_err(|e| e.to_string())?;
        s.validate()?;
        Ok(s)
    }

    pub fn validate(&self) -> Result<(), String> {
        if !(1..=240).contains(&self.fps) {
            return Err(format!("fps {} out of range 1..=240", self.fps));
        }
        if !(100..=500_000).contains(&self.bitrate_kbps) {
            return Err(format!("bitrate_kbps {} out of range", self.bitrate_kbps));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_x264() {
        let s = EncoderSettings::parse("").unwrap();
        assert_eq!(s.encoder, EncoderKind::X264);
        assert_eq!(s.x264.preset, "superfast");
        assert_eq!(s.x264.tune, "zerolatency");
    }

    #[test]
    fn parses_user_choice() {
        let s = EncoderSettings::parse("encoder = \"openh264\"\nbitrate_kbps = 8000\n[x264]\npreset = \"ultrafast\"\n").unwrap();
        assert_eq!(s.encoder, EncoderKind::OpenH264);
        assert_eq!(s.bitrate_kbps, 8000);
        assert_eq!(s.x264.preset, "ultrafast");
        assert_eq!(EncoderSettings::parse("encoder = \"mf\"").unwrap().encoder, EncoderKind::MediaFoundation);
    }

    #[test]
    fn rejects_garbage() {
        assert!(EncoderSettings::parse("encoder = \"av1-magic\"").is_err());
        assert!(EncoderSettings::parse("fps = 0").is_err());
    }
}
