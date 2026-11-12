//! Server identity: a self-signed certificate created on first start and kept in
//! %ProgramData%\FastRDP\server (the installer restricts that directory to SYSTEM, Administrators
//! and LOCAL SERVICE). Clients pin the SHA-256 fingerprint published in `cert.sha256`.
use std::path::Path;

use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use sha2::{Digest, Sha256};

pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02x}")).collect()
}

pub fn load_or_create(dir: &Path) -> std::io::Result<(CertificateDer<'static>, PrivateKeyDer<'static>, String)> {
    std::fs::create_dir_all(dir)?;
    let (cp, kp) = (dir.join("cert.der"), dir.join("key.der"));
    if !(cp.exists() && kp.exists()) {
        let host = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "fastrdp".into());
        let ck = rcgen::generate_simple_self_signed(vec![host, "fastrdp".into()]).map_err(std::io::Error::other)?;
        std::fs::write(&kp, ck.signing_key.serialize_der())?;
        std::fs::write(&cp, ck.cert.der().as_ref())?;
    }
    let cert = std::fs::read(&cp)?;
    let key = std::fs::read(&kp)?;
    let fp = fingerprint(&cert);
    std::fs::write(dir.join("cert.sha256"), format!("{fp}\n"))?;
    Ok((CertificateDer::from(cert), PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)), fp))
}
