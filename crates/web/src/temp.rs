//! Chromium's temp dir for the host. Chromium makes temp dirs for component
//! updater downloads (`<bundle id>.chromium_chrome_url_fetcher_.*`), their
//! unpacking (`….Unpacker_BeginUnzipping.*`) and the profile singleton's
//! socket, and leaves them behind when the host exits mid-task. In the system
//! temp dir nothing would ever remove them, so the host points Chromium's temp
//! dir ([`ENV`]) at `<temp dir>/ghostrealm-<uid>-cef/<host pid>`: [`HostTemp`]
//! removes it when the host exits, and the next host removes those whose host
//! is gone.
//!
//! The dir is in the system temp dir rather than under the profile because the
//! singleton's socket path must fit [`SOCKET_PATH_MAX`], whatever the profile's
//! path.

use std::io;
use std::path::{Path, PathBuf};

use crate::frame::{private_dir, user_id};

/// The variable Chromium's `base::GetTempDir` reads first: on macOS
/// `MAC_CHROMIUM_TMPDIR` (it takes no notice of `TMPDIR` there), elsewhere
/// `TMPDIR`. Child processes inherit it. Off macOS that includes anything else
/// the host or Chromium spawns (an external protocol handler run through
/// `xdg-open`, say), whose temp dir then goes when the host exits even if it
/// still runs.
pub const ENV: &str = if cfg!(target_os = "macos") {
    "MAC_CHROMIUM_TMPDIR"
} else {
    "TMPDIR"
};

/// The host bundle's id, which Chromium names its temp dirs after on macOS.
pub const BUNDLE_ID: &str = "dev.ghostrealm.web-host";

/// The longest socket path Chromium's profile singleton accepts: on macOS
/// `SOCK_MAXADDRLEN` (255) less the address header (Chromium aborts with
/// "Socket path too long" past it); elsewhere `sun_path` (108) less its NUL.
pub const SOCKET_PATH_MAX: usize = if cfg!(target_os = "macos") { 253 } else { 107 };

/// The most digits a pid has.
const PID_DIGITS_MAX: usize = u32::MAX.ilog10() as usize + 1;

/// How much Chromium adds to its temp dir for the singleton's socket:
/// `/<name>.XXXXXX/SingletonSocket`, the name being the bundle id on macOS.
fn socket_suffix_len() -> usize {
    let name = if cfg!(target_os = "macos") {
        BUNDLE_ID
    } else {
        ".org.chromium.Chromium"
    };
    1 + name.len() + ".XXXXXX".len() + "/SingletonSocket".len()
}

/// `<temp>/ghostrealm-<uid>-cef`, which holds each host's temp dir.
fn base_dir(temp: &Path) -> PathBuf {
    temp.join(format!("ghostrealm-{}-cef", user_id()))
}

/// Whether the singleton's socket fits [`SOCKET_PATH_MAX`] under any host's
/// temp dir in `base`.
fn fits(base: &Path) -> bool {
    base.as_os_str().len() + 1 + PID_DIGITS_MAX + socket_suffix_len() <= SOCKET_PATH_MAX
}

/// This host's temp dir, removed with everything in it on drop.
#[derive(Debug)]
pub struct HostTemp(PathBuf);

impl HostTemp {
    /// Create this process's temp dir in `<temp>/ghostrealm-<uid>-cef` (both
    /// mode 0700), emptying any an earlier process with this pid left, after
    /// removing those of hosts no longer `running`. Fails when the socket
    /// might not fit or that dir is not private to this user.
    ///
    /// The pid sweep is safe without holding the profile: the dir is private,
    /// so only this user's hosts add to it, each naming its dir after its own
    /// pid once it runs, so a running host's dir is never removed whichever
    /// profile it is on.
    pub fn create(temp: &Path, running: impl Fn(u32) -> bool) -> io::Result<Self> {
        let base = base_dir(temp);
        if !fits(&base) {
            return Err(io::Error::other(format!(
                "a socket under {} could exceed {SOCKET_PATH_MAX} bytes",
                base.display()
            )));
        }
        private_dir(&base)?;
        remove_stale(&base, running);
        let dir = base.join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        private_dir(&dir)?;
        Ok(Self(dir))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for HostTemp {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Remove each `<pid>` dir in `base` whose host is no longer `running`. Anything
/// else in `base` is left alone.
fn remove_stale(base: &Path, running: impl Fn(u32) -> bool) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        if !running(pid) && entry.file_type().is_ok_and(|t| t.is_dir()) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ghostrealm-host-temp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        root
    }

    /// Given the system temp dir, and the longest uid and pid;
    /// then the singleton's socket path under a host's temp dir fits.
    #[test]
    fn the_singleton_socket_fits_under_the_default_temp_dir() {
        let worst = |temp: &str| {
            let dir = Path::new(temp).join(format!("ghostrealm-{}-cef", u32::MAX)).join(u32::MAX.to_string());
            dir.as_os_str().len() + socket_suffix_len()
        };
        // The macOS default (`/var/folders/<2>/<28>/T/`) and `/tmp`.
        let default = if cfg!(target_os = "macos") {
            "/var/folders/h5/929361pj77g8r3jlntl4c8d00000gn/T/"
        } else {
            "/tmp"
        };
        assert!(worst(default) <= SOCKET_PATH_MAX, "{} > {SOCKET_PATH_MAX}", worst(default));
        assert!(fits(&Path::new(default).join(format!("ghostrealm-{}-cef", u32::MAX))));
        let here = std::env::temp_dir();
        assert_eq!(fits(&base_dir(&here)), worst(here.to_str().unwrap()) <= SOCKET_PATH_MAX);
        assert!(fits(&base_dir(&here)), "this machine's temp dir {} leaves no room", here.display());
    }

    /// Given a temp dir so long the singleton's socket might not fit;
    /// then no host temp dir is made there.
    #[test]
    fn a_temp_dir_too_long_for_the_socket_is_refused() {
        let root = root("long");
        let long = root.join("a".repeat(SOCKET_PATH_MAX));
        assert!(HostTemp::create(&long, |_| false).is_err());
        assert!(!long.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Given temp dirs of a gone host, a running host, a gone host that had
    /// this pid, and entries not named after a pid;
    /// when a host creates its temp dir there;
    /// then the gone hosts' dirs are removed, the others kept, its own dir is
    /// created empty, and dropping it removes it.
    #[test]
    fn a_host_clears_gone_hosts_temp_dirs_and_removes_its_own() {
        let root = root("sweep");
        let base = base_dir(&root);
        let gone = base.join("100");
        let live = base.join("200");
        let mine = base.join(std::process::id().to_string());
        let other = base.join("notes");
        let file = base.join("300");
        for d in [&gone, &live, &mine, &other] {
            std::fs::create_dir_all(d.join("dev.ghostrealm.web-host.chromium_chrome_url_fetcher_.x")).unwrap();
        }
        std::fs::write(&file, b"").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let temp = HostTemp::create(&root, |pid| pid == 200 || pid == std::process::id()).unwrap();
        assert_eq!(temp.path(), mine);
        assert!(!gone.exists(), "a gone host's dir is removed");
        assert!(live.join("dev.ghostrealm.web-host.chromium_chrome_url_fetcher_.x").exists());
        assert!(other.exists() && file.exists(), "entries not named after a host are kept");
        assert_eq!(std::fs::read_dir(&mine).unwrap().count(), 0, "its own dir starts empty");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!((mode(&base), mode(&mine)), (0o700, 0o700));
        }

        std::fs::create_dir(mine.join("dev.ghostrealm.web-host.Unpacker_BeginUnzipping.y")).unwrap();
        drop(temp);
        assert!(!mine.exists(), "dropping it removes it and its contents");
        assert!(live.exists());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Given a host temp base dir others can open;
    /// then no host temp dir is made in it.
    #[cfg(unix)]
    #[test]
    fn a_base_dir_others_can_open_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = root("open");
        let base = base_dir(&root);
        std::fs::create_dir(&base).unwrap();
        std::fs::set_permissions(&base, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(HostTemp::create(&root, |_| false).is_err());
        assert_eq!(std::fs::read_dir(&base).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
