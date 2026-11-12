//! Client updates: at start the latest signed release is looked up (`update` crate); on request the
//! client package is downloaded, verified, and the running executable is replaced (a running exe
//! can be renamed, not overwritten), then the new version is started.
use update::Release;

/// The latest release if newer than this client (errors are only logged).
pub fn check() -> Option<Release> {
    match update::newer() {
        Ok(r) => r,
        Err(e) => {
            crate::log::line(&format!("update check: {e}"));
            None
        }
    }
}

fn old_path(exe: &std::path::Path) -> std::path::PathBuf {
    exe.with_extension("exe.old")
}

/// Leftover of the previous update.
pub fn cleanup() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = std::fs::remove_file(old_path(&exe));
    }
}

/// Downloads and verifies the client package, swaps the executable and starts the new version.
/// The caller exits afterwards.
pub fn install(release: &Release) -> Result<(), String> {
    if !cfg!(windows) {
        return Err(format!("download FastRDP {} from {}", release.version, release.url(&release.client_package())));
    }
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let dir = std::env::temp_dir().join(format!("fastrdp-client-update-{}", release.version));
    let _ = std::fs::remove_dir_all(&dir);
    let zip = update::download(release, &release.client_package(), &dir).map_err(|e| e.to_string())?;
    update::extract(&zip, &dir).map_err(|e| e.to_string())?;
    let new = dir.join(release.client_package().trim_end_matches(".zip")).join("fastrdp-client.exe");
    if !new.is_file() {
        return Err("the client package has no fastrdp-client.exe".into());
    }
    let old = old_path(&exe);
    let _ = std::fs::remove_file(&old);
    std::fs::rename(&exe, &old).map_err(|e| format!("cannot replace {}: {e} - download the new version from the releases page", exe.display()))?;
    if let Err(e) = std::fs::copy(&new, &exe) {
        let _ = std::fs::rename(&old, &exe);
        return Err(format!("cannot write {}: {e}", exe.display()));
    }
    let _ = std::fs::remove_dir_all(&dir);
    crate::log::line(&format!("updated to {}, restarting", release.version));
    std::process::Command::new(&exe).spawn().map_err(|e| format!("updated, but could not start it: {e}"))?;
    Ok(())
}
