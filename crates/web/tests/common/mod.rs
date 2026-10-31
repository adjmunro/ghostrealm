//! Shared by the tests that run the real CEF host.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use ghostrealm_web::client::{HOST_ENV, HOST_NAME};
use ghostrealm_web::WebHost;

/// The bundled host (`mise run web`), if it has been built, unless
/// `GHOSTREALM_NO_WEB` is set: such a build leaves whatever bundle an earlier
/// one placed.
pub fn host_exe() -> Option<PathBuf> {
    if std::env::var("GHOSTREALM_NO_WEB").is_ok_and(|v| !v.is_empty() && v != "0") {
        return None;
    }
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

/// Whether process `pid` is still running.
pub fn running(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Hang up on `host` and remove its profile `cache` once it has exited: it
/// writes there until it does, so removing it sooner leaves it behind.
#[allow(dead_code)] // Not every test crate drops a host this way.
pub fn shut_down(host: WebHost, cache: &Path) {
    let pid = host.pid();
    drop(host);
    // Its reaper kills it after 5 s.
    let until = Instant::now() + Duration::from_secs(10);
    while pid.is_some_and(running) && Instant::now() < until {
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = std::fs::remove_dir_all(cache);
}
