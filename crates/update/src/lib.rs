//! FastRDP updates.
//!
//! Every GitHub release carries `manifest.json` (version, tag, SHA-256 and size of each release
//! file) and `manifest.json.sig` (`ed25519 <key id> <base64 signature>` over the exact manifest
//! bytes). The signature is made by the release workflow with a key whose public half is built in
//! (`keys.rs`), so a changed release file, a tampered download or a compromised web front-end cannot
//! produce an update that is accepted. Only versions newer than the running one are offered.
use std::collections::BTreeMap;
use std::fmt;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use sha2::{Digest, Sha256};

mod keys;
pub use keys::TRUSTED_KEYS;

pub const REPO: &str = "othelot/fastrdp";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_FILE: u64 = 512 * 1024 * 1024;

#[derive(Debug)]
pub enum Error {
    Network(String),
    Signature(String),
    Manifest(String),
    Integrity(String),
    Io(std::io::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Network(e) => write!(f, "network: {e}"),
            Error::Signature(e) => write!(f, "signature: {e}"),
            Error::Manifest(e) => write!(f, "manifest: {e}"),
            Error::Integrity(e) => write!(f, "download check failed: {e}"),
            Error::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

/// `major.minor.patch` (pre-release versions are never offered as updates).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version(pub u64, pub u64, pub u64);

impl Version {
    pub fn parse(s: &str) -> Option<Version> {
        let mut it = s.trim().trim_start_matches('v').split('.');
        let v = Version(it.next()?.parse().ok()?, it.next()?.parse().ok()?, it.next()?.parse().ok()?);
        it.next().is_none().then_some(v)
    }

    pub fn current() -> Version {
        Version::parse(CURRENT).expect("crate version is major.minor.patch")
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub sha256: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct RawManifest {
    format: u32,
    version: String,
    tag: String,
    files: BTreeMap<String, FileEntry>,
}

/// A verified release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Release {
    pub version: Version,
    pub tag: String,
    pub files: BTreeMap<String, FileEntry>,
}

impl Release {
    pub fn url(&self, name: &str) -> String {
        format!("https://github.com/{REPO}/releases/download/{}/{name}", self.tag)
    }

    /// Server package (`fastrdp-<v>-x64.zip`) / client package (`fastrdp-client-<v>-x64.zip`).
    pub fn server_package(&self) -> String {
        format!("fastrdp-{}-x64.zip", self.version)
    }

    pub fn client_package(&self) -> String {
        format!("fastrdp-client-{}-x64.zip", self.version)
    }
}

fn key_id(public: &[u8; 32]) -> String {
    Sha256::digest(public)[..8].iter().map(|b| format!("{b:02x}")).collect()
}

/// Checks `sig` (`ed25519 <key id> <base64>`) over the exact `manifest` bytes with one of `keys`
/// (base64 raw Ed25519 public keys) and parses the manifest.
pub fn verify_manifest(manifest: &[u8], sig: &str, keys: &[&str]) -> Result<Release, Error> {
    let b64 = base64::engine::general_purpose::STANDARD;
    let mut parts = sig.split_whitespace();
    let (Some("ed25519"), Some(id), Some(sig_b64), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(Error::Signature("unrecognised signature file".into()));
    };
    let sig_bytes: [u8; 64] = b64.decode(sig_b64).ok().and_then(|v| v.try_into().ok()).ok_or_else(|| Error::Signature("bad signature encoding".into()))?;
    if keys.is_empty() {
        return Err(Error::Signature("this build has no trusted release keys".into()));
    }
    let key = keys
        .iter()
        .filter_map(|k| b64.decode(k).ok().and_then(|v| <[u8; 32]>::try_from(v).ok()))
        .find(|k| key_id(k) == id)
        .ok_or_else(|| Error::Signature(format!("signed with unknown key {id}")))?;
    let vk = ed25519_dalek::VerifyingKey::from_bytes(&key).map_err(|e| Error::Signature(e.to_string()))?;
    vk.verify_strict(manifest, &ed25519_dalek::Signature::from_bytes(&sig_bytes)).map_err(|_| Error::Signature("signature does not match".into()))?;

    let raw: RawManifest = serde_json::from_slice(manifest).map_err(|e| Error::Manifest(e.to_string()))?;
    if raw.format != 1 {
        return Err(Error::Manifest(format!("unsupported format {}", raw.format)));
    }
    let version = Version::parse(&raw.version).ok_or_else(|| Error::Manifest(format!("bad version '{}'", raw.version)))?;
    if raw.tag != format!("v{version}") {
        return Err(Error::Manifest(format!("tag '{}' does not match version {version}", raw.tag)));
    }
    for (name, f) in &raw.files {
        let ok_name = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !name.starts_with('.');
        if !ok_name || f.sha256.len() != 64 || !f.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(Error::Manifest(format!("bad file entry '{name}'")));
        }
    }
    Ok(Release { version, tag: raw.tag, files: raw.files })
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(600)))
        .timeout_connect(Some(Duration::from_secs(20)))
        .user_agent(format!("fastrdp-update/{CURRENT}"))
        .build()
        .into()
}

fn get_small(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
    let mut r = agent.get(url).call().map_err(|e| Error::Network(format!("{url}: {e}")))?;
    let mut v = Vec::new();
    r.body_mut().as_reader().take(MAX_MANIFEST + 1).read_to_end(&mut v).map_err(|e| Error::Network(e.to_string()))?;
    if v.len() as u64 > MAX_MANIFEST {
        return Err(Error::Manifest("too large".into()));
    }
    Ok(v)
}

/// The latest published release, verified.
pub fn latest() -> Result<Release, Error> {
    let agent = agent();
    let base = format!("https://github.com/{REPO}/releases/latest/download");
    let manifest = get_small(&agent, &format!("{base}/manifest.json"))?;
    let sig = get_small(&agent, &format!("{base}/manifest.json.sig"))?;
    verify_manifest(&manifest, &String::from_utf8_lossy(&sig), TRUSTED_KEYS)
}

/// The latest release if it is newer than this build.
pub fn newer() -> Result<Option<Release>, Error> {
    let r = latest()?;
    Ok((r.version > Version::current()).then_some(r))
}

/// Downloads `name` of `release` into `dir` and checks size and SHA-256 (the file is deleted on mismatch).
pub fn download(release: &Release, name: &str, dir: &Path) -> Result<PathBuf, Error> {
    let entry = release.files.get(name).ok_or_else(|| Error::Manifest(format!("{name} is not part of release {}", release.version)))?;
    if entry.size > MAX_FILE {
        return Err(Error::Manifest(format!("{name} too large")));
    }
    std::fs::create_dir_all(dir)?;
    let path = dir.join(name);
    let res = (|| {
        let mut r = agent().get(&release.url(name)).call().map_err(|e| Error::Network(format!("{name}: {e}")))?;
        let mut reader = r.body_mut().with_config().limit(entry.size + 1).reader();
        let mut out = std::fs::File::create(&path)?;
        let mut hash = Sha256::new();
        let mut total = 0u64;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = reader.read(&mut buf).map_err(|e| Error::Network(e.to_string()))?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > entry.size {
                return Err(Error::Integrity(format!("{name}: larger than announced")));
            }
            hash.update(&buf[..n]);
            out.write_all(&buf[..n])?;
        }
        out.sync_all()?;
        let got: String = hash.finalize().iter().map(|b| format!("{b:02x}")).collect();
        if total != entry.size || !got.eq_ignore_ascii_case(&entry.sha256) {
            return Err(Error::Integrity(format!("{name}: size {total} sha256 {got}, expected {} {}", entry.size, entry.sha256)));
        }
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&path);
    }
    res.map(|()| path)
}

/// Extracts a (verified) zip into `dest`. Entries outside `dest` are rejected.
pub fn extract(zip_path: &Path, dest: &Path) -> Result<(), Error> {
    let mut z = zip::ZipArchive::new(std::fs::File::open(zip_path)?).map_err(|e| Error::Integrity(e.to_string()))?;
    for i in 0..z.len() {
        let mut f = z.by_index(i).map_err(|e| Error::Integrity(e.to_string()))?;
        let rel = f.enclosed_name().ok_or_else(|| Error::Integrity(format!("unsafe path {}", f.name())))?;
        let out = dest.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::io::copy(&mut f, &mut std::fs::File::create(&out)?)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Produced with OpenSSL exactly like .github/workflows/sign-release.yml does (throw-away key):
    //   openssl genpkey -algorithm ed25519 -out k.pem
    //   openssl pkeyutl -sign -inkey k.pem -rawin -in manifest.json -out sig.raw
    const TEST_KEY: &str = "RZDLtT7tO5M6ySAsEj+Fts6C8JT7gDgkJocVN7msJCs=";
    const TEST_SIG: &str = "ed25519 a143b866078c960c 0k6jT0JVPm1Ameaz++biTPjNED2JI8FTX20WXGNe2oAr1Z65gHAnd2ovfPlV6xYXNkwCd+Qx3A+lfVL4HSJrBg==";
    const TEST_MANIFEST: &str = r#"{
  "format": 1,
  "version": "0.2.0",
  "tag": "v0.2.0",
  "files": {
    "fastrdp-0.2.0-x64.zip": {
      "sha256": "abababababababababababababababababababababababababababababababab",
      "size": 123
    },
    "install.ps1": {
      "sha256": "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd",
      "size": 45
    }
  }
}
"#;

    /// exact signed bytes (LF), whatever line endings git checked this file out with
    fn manifest() -> String {
        TEST_MANIFEST.replace("\r\n", "\n")
    }

    #[test]
    fn openssl_signature_is_accepted() {
        let r = verify_manifest(manifest().as_bytes(), TEST_SIG, &[TEST_KEY]).unwrap();
        assert_eq!(r.version, Version(0, 2, 0));
        assert_eq!(r.tag, "v0.2.0");
        assert_eq!(r.server_package(), "fastrdp-0.2.0-x64.zip");
        assert_eq!(r.url("install.ps1"), "https://github.com/othelot/fastrdp/releases/download/v0.2.0/install.ps1");
        assert_eq!(r.files["fastrdp-0.2.0-x64.zip"].size, 123);
    }

    #[test]
    fn tampering_is_rejected() {
        let changed = manifest().replace("123", "124");
        assert!(matches!(verify_manifest(changed.as_bytes(), TEST_SIG, &[TEST_KEY]), Err(Error::Signature(_))));
        // unknown key
        let other = base64::engine::general_purpose::STANDARD.encode([7u8; 32]);
        assert!(matches!(verify_manifest(manifest().as_bytes(), TEST_SIG, &[&other]), Err(Error::Signature(_))));
        assert!(matches!(verify_manifest(manifest().as_bytes(), TEST_SIG, &[]), Err(Error::Signature(_))));
        assert!(verify_manifest(manifest().as_bytes(), "garbage", &[TEST_KEY]).is_err());
    }

    #[test]
    fn built_in_keys_are_valid() {
        let b64 = base64::engine::general_purpose::STANDARD;
        let ids: Vec<String> = TRUSTED_KEYS
            .iter()
            .map(|k| {
                let raw: [u8; 32] = b64.decode(k).unwrap().try_into().unwrap();
                ed25519_dalek::VerifyingKey::from_bytes(&raw).expect("valid Ed25519 public key");
                key_id(&raw)
            })
            .collect();
        assert_eq!(ids, ["118f186f6fb6a31e", "61fa02da77880474"], "same ids as `openssl ... | sha256sum`");
    }

    #[test]
    fn versions() {
        assert_eq!(Version::parse("v1.2.3"), Some(Version(1, 2, 3)));
        assert_eq!(Version::parse("1.2"), None);
        assert_eq!(Version::parse("1.2.3-rc1"), None);
        assert!(Version(0, 10, 0) > Version(0, 9, 9));
    }
}
