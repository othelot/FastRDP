// Builds the x264 C shim and links a static libx264 (GPLv2).
// libx264 location: FASTRDP_X264_DIR (containing include/x264.h and lib/libx264.lib), else
// <vcpkg>/installed/x64-windows-static with <vcpkg> = %VCPKG_ROOT% or %USERPROFILE%\vcpkg
// (`vcpkg install x264:x64-windows-static`).
use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=shim/fx264.c");
    println!("cargo:rerun-if-env-changed=FASTRDP_X264_DIR");
    println!("cargo:rerun-if-env-changed=VCPKG_ROOT");
    println!("cargo:rerun-if-env-changed=USERPROFILE");
    let dir = std::env::var_os("FASTRDP_X264_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let vcpkg = std::env::var_os("VCPKG_ROOT")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join("vcpkg")))
            .unwrap_or_else(|| PathBuf::from("vcpkg"));
        vcpkg.join("installed").join("x64-windows-static")
    });
    let inc = dir.join("include");
    let lib = dir.join("lib");
    if !inc.join("x264.h").exists() || !lib.join("libx264.lib").exists() {
        panic!(
            "libx264 not found in {}.\nx264 is the mandatory fallback encoder. Build it with:\n  \
             vcpkg install x264:x64-windows-static\nwith vcpkg in %USERPROFILE%\\vcpkg (or set VCPKG_ROOT / FASTRDP_X264_DIR).",
            dir.display()
        );
    }
    cc::Build::new().file("shim/fx264.c").include(&inc).static_crt(true).warnings(true).compile("fx264");
    println!("cargo:rustc-link-search=native={}", lib.display());
    println!("cargo:rustc-link-lib=static=libx264");
}
