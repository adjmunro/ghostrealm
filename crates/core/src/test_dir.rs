//! Temp directories for tests.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// A unique, empty temp directory, removed when dropped (also when the test
/// fails).
#[derive(Debug)]
pub(crate) struct TmpDir(PathBuf);

impl TmpDir {
    pub(crate) fn new(tag: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ghostrealm-{tag}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TmpDir(dir)
    }
}

impl std::ops::Deref for TmpDir {
    type Target = Path;

    fn deref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<Path> for TmpDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl AsRef<std::ffi::OsStr> for TmpDir {
    fn as_ref(&self) -> &std::ffi::OsStr {
        self.0.as_os_str()
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
