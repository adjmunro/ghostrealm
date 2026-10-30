//! Chromium's temp dir for the host. Chromium makes temp dirs for component
//! updater downloads (`<bundle id>.chromium_chrome_url_fetcher_.*`), their
//! unpacking (`….Unpacker_BeginUnzipping.*`) and the profile singleton's
//! socket, and leaves them behind when the host exits mid-task. In the system
//! temp dir nothing would ever remove them, so the host points Chromium's temp
//! dir ([`ENV`]) at `<profile root>/tmp/<host pid>`: [`HostTemp`] removes it
//! when the host exits, and the next host on the profile removes those whose
//! host is gone.

use std::io;
use std::path::{Path, PathBuf};

/// The variable Chromium's `base::GetTempDir` reads first: on macOS
/// `MAC_CHROMIUM_TMPDIR` (it takes no notice of `TMPDIR` there), elsewhere
/// `TMPDIR`. Child processes inherit it.
pub const ENV: &str = if cfg!(target_os = "macos") {
    "MAC_CHROMIUM_TMPDIR"
} else {
    "TMPDIR"
};

/// This host's temp dir, `<root>/tmp/<pid>`, removed with everything in it on
/// drop.
#[derive(Debug)]
pub struct HostTemp(PathBuf);

impl HostTemp {
    /// Remove the temp dirs under `root` of hosts no longer `running`, then
    /// create this process's (mode 0700), emptying any an earlier process with
    /// this pid left.
    pub fn create(root: &Path, running: impl Fn(u32) -> bool) -> io::Result<Self> {
        let base = root.join("tmp");
        remove_stale(&base, running);
        let dir = base.join(std::process::id().to_string());
        let _ = std::fs::remove_dir_all(&dir);
        create_private(&dir)?;
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

#[cfg(unix)]
fn create_private(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

#[cfg(not(unix))]
fn create_private(dir: &Path) -> io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Given a profile root with temp dirs of a gone host, a running host, a
    /// gone host that had this pid, and entries not named after a pid;
    /// when a host creates its temp dir there;
    /// then the gone hosts' dirs are removed, the others kept, its own dir is
    /// created empty, and dropping it removes it.
    #[test]
    fn a_host_clears_gone_hosts_temp_dirs_and_removes_its_own() {
        let root = std::env::temp_dir().join(format!("ghostrealm-host-temp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let base = root.join("tmp");
        let gone = base.join("100");
        let live = base.join("200");
        let mine = base.join(std::process::id().to_string());
        let other = base.join("notes");
        let file = base.join("300");
        for d in [&gone, &live, &mine, &other] {
            std::fs::create_dir_all(d.join("dev.ghostrealm.web-host.chromium_chrome_url_fetcher_.x")).unwrap();
        }
        std::fs::write(&file, b"").unwrap();

        let temp = HostTemp::create(&root, |pid| pid == 200 || pid == std::process::id()).unwrap();
        assert_eq!(temp.path(), mine);
        assert!(!gone.exists(), "a gone host's dir is removed");
        assert!(live.join("dev.ghostrealm.web-host.chromium_chrome_url_fetcher_.x").exists());
        assert!(other.exists() && file.exists(), "entries not named after a host are kept");
        assert_eq!(std::fs::read_dir(&mine).unwrap().count(), 0, "its own dir starts empty");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&mine).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }

        std::fs::create_dir(mine.join("dev.ghostrealm.web-host.Unpacker_BeginUnzipping.y")).unwrap();
        drop(temp);
        assert!(!mine.exists(), "dropping it removes it and its contents");
        assert!(live.exists());
        let _ = std::fs::remove_dir_all(&root);
    }
}
