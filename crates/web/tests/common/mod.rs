//! Shared by the tests that run the real CEF host.

use std::path::PathBuf;

use ghostrealm_web::client::{HOST_ENV, HOST_NAME};

/// The bundled host (`mise run web`), if it has been built.
pub fn host_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(HOST_ENV) {
        return Some(PathBuf::from(p));
    }
    // Test binaries live in target/<profile>/deps; the bundle sits in target/<profile>.
    let profile_dir = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let mac = profile_dir.join(format!("{HOST_NAME}.app/Contents/MacOS/{HOST_NAME}"));
    let linux = profile_dir.join(HOST_NAME).join(HOST_NAME);
    [mac, linux].into_iter().find(|p| p.is_file())
}
