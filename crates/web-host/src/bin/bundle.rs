//! Assemble the runnable web host from the built binaries in this executable's
//! own target directory, placing it where `ghostrealm` looks (beside itself):
//! `ghostrealm-web-host.app` on macOS (CEF framework + helper apps inside), a
//! `ghostrealm-web-host/` directory (binary + CEF libraries) on Linux.
//!
//! Build the binaries first: `cargo build -p ghostrealm-web-host --bins`.

use std::path::PathBuf;

const HOST: &str = "ghostrealm-web-host";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let target: PathBuf = std::env::current_exe()?
        .parent()
        .ok_or("executable has no parent directory")?
        .to_path_buf();
    let out = bundle(&target)?;
    println!("web host bundled at {}", out.display());
    Ok(())
}

#[cfg(target_os = "macos")]
fn bundle(target: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    use cef::build_util::mac::{bundle, BundleInfo};
    let info = BundleInfo::new(
        HOST,
        ghostrealm_web::temp::BUNDLE_ID,
        HOST,
        "English",
        semver::Version::new(0, 0, 0),
    );
    let app = target.join(format!("{HOST}.app"));
    if app.exists() {
        std::fs::remove_dir_all(&app)?;
    }
    Ok(bundle(
        target,
        target,
        HOST,
        &format!("{HOST}-helper"),
        None,
        info,
    )?)
}

#[cfg(target_os = "linux")]
fn bundle(target: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Ok(cef::build_util::linux::bundle(
        &target.join(HOST),
        target,
        HOST,
    )?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn bundle(_target: &std::path::Path) -> Result<PathBuf, Box<dyn std::error::Error>> {
    Err("the web host is only bundled on macOS and Linux".into())
}
