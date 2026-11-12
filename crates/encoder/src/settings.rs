//! User-configurable encoder settings (`%ProgramData%\FastRDP\encoder.toml`).
//!
//! ```toml
//! encoder = "openh264"      # openh264 (default) | nvenc | mf (not implemented yet -> openh264)
//!                           # ("x264", removed in 0.1.6, reads as openh264)
//! bitrate_kbps = 15000     # at 1920x1080@60; scaled to the session's size and refresh rate,
//!                           # unless the client sets its own target
//! fps = 60                  # the streaming server uses the client's refresh rate instead
//! keyint_secs = 0           # 0 = no periodic IDR (IDR only on request / fallback)
//! retry_requested_secs = 30 # after a fallback, retry the configured encoder every N s (0 = never)
//!
//! [openh264]
//! usage = "camera"          # camera (fast, multi-threaded) | screen (fewer bits on static
//!                           # desktops, single-threaded and ~3x slower)
//! complexity = "medium"     # low | medium | high
//! threads = 0               # 0 = automatic (frames split into slices, one thread each), 1 = single
//!
//! [nvenc]                   # NVIDIA GPU (driver 522+); unavailable -> openh264
//! preset = "p1"             # p1 (fastest, lowest latency) .. p7 (best quality)
//! ```
//! Whatever is configured, **OpenH264 is the fallback** when that encoder is missing or fails.

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum EncoderKind {
    #[default]
    #[serde(alias = "x264")]
    OpenH264,
    Nvenc,
    #[serde(rename = "mf")]
    MediaFoundation,
}

impl fmt::Display for EncoderKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
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
            "openh264" | "x264" => Ok(EncoderKind::OpenH264),
            "nvenc" => Ok(EncoderKind::Nvenc),
            "mf" | "mediafoundation" => Ok(EncoderKind::MediaFoundation),
            o => Err(format!("unknown encoder '{o}' (openh264|nvenc|mf)")),
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct OpenH264Settings {
    pub usage: String,
    pub complexity: String,
    pub threads: u16,
}

impl Default for OpenH264Settings {
    fn default() -> Self {
        OpenH264Settings { usage: "camera".into(), complexity: "medium".into(), threads: 0 }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(default)]
pub struct NvencSettings {
    pub preset: String,
}

impl Default for NvencSettings {
    fn default() -> Self {
        NvencSettings { preset: "p1".into() }
    }
}

impl NvencSettings {
    /// 1..=7 from "p1".."p7".
    pub fn preset_number(&self) -> Result<usize, String> {
        self.preset
            .trim()
            .to_ascii_lowercase()
            .strip_prefix('p')
            .and_then(|n| n.parse::<usize>().ok())
            .filter(|n| (1..=7).contains(n))
            .ok_or_else(|| format!("nvenc preset '{}' (p1..p7)", self.preset))
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
    pub openh264: OpenH264Settings,
    pub nvenc: NvencSettings,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        EncoderSettings {
            encoder: EncoderKind::OpenH264,
            bitrate_kbps: 15_000,
            fps: 60,
            keyint_secs: 0,
            retry_requested_secs: 30,
            openh264: OpenH264Settings::default(),
            nvenc: NvencSettings::default(),
        }
    }
}

impl EncoderSettings {
    pub fn default_path() -> PathBuf {
        let pd = std::env::var("ProgramData").unwrap_or_else(|_| r"C:\ProgramData".into());
        Path::new(&pd).join("FastRDP").join("encoder.toml")
    }

    /// Missing file -> defaults (OpenH264). Malformed file -> error (never silently ignore user config).
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
        self.nvenc.preset_number()?;
        if !["low", "medium", "high"].contains(&self.openh264.complexity.to_ascii_lowercase().as_str()) {
            return Err(format!("openh264 complexity '{}' (low|medium|high)", self.openh264.complexity));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_openh264() {
        let s = EncoderSettings::parse("").unwrap();
        assert_eq!(s.encoder, EncoderKind::OpenH264);
        assert_eq!(s.openh264.usage, "camera");
        assert_eq!(s.openh264.complexity, "medium");
    }

    #[test]
    fn parses_user_choice() {
        let s = EncoderSettings::parse("encoder = \"nvenc\"\nbitrate_kbps = 8000\n[openh264]\ncomplexity = \"low\"\n").unwrap();
        assert_eq!(s.encoder, EncoderKind::Nvenc);
        assert_eq!(s.bitrate_kbps, 8000);
        assert_eq!(s.openh264.complexity, "low");
        assert_eq!(EncoderSettings::parse("encoder = \"mf\"").unwrap().encoder, EncoderKind::MediaFoundation);
    }

    #[test]
    fn files_written_for_x264_still_load() {
        let s = EncoderSettings::parse("encoder = \"x264\"\n[x264]\npreset = \"superfast\"\n").unwrap();
        assert_eq!(s.encoder, EncoderKind::OpenH264);
        assert_eq!("x264".parse::<EncoderKind>(), Ok(EncoderKind::OpenH264));
    }

    #[test]
    fn rejects_garbage() {
        assert!(EncoderSettings::parse("encoder = \"av1-magic\"").is_err());
        assert!(EncoderSettings::parse("fps = 0").is_err());
        assert!(EncoderSettings::parse("[nvenc]\npreset = \"p9\"").is_err());
        assert!(EncoderSettings::parse("[openh264]\ncomplexity = \"max\"").is_err());
        assert_eq!(EncoderSettings::parse("encoder = \"nvenc\"\n[nvenc]\npreset = \"P4\"").unwrap().nvenc.preset_number(), Ok(4));
    }
}
