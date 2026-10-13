//! What identifies one version of a file on disk, for noticing that another
//! program wrote it: modification time, size, and (on Unix) inode, so a file
//! replaced by an atomic rename-over is seen even when it kept the same time
//! and size.

use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    inode: Option<u64>,
}

impl Stamp {
    /// The file at `path` as it is now; `None` while it is absent.
    pub fn of(path: &Path) -> Option<Stamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Stamp { modified: meta.modified().ok(), len: meta.len(), inode: inode(&meta) })
    }
}

#[cfg(unix)]
fn inode(meta: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    Some(meta.ino())
}

#[cfg(not(unix))]
fn inode(_: &std::fs::Metadata) -> Option<u64> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;

    #[test]
    fn a_file_replaced_by_rename_with_the_same_time_and_size_differs() {
        let dir = tmpdir("stamp");
        let path = dir.join("f.txt");
        std::fs::write(&path, "a = 1\n").unwrap();
        let before = Stamp::of(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        let tmp = dir.join("f.tmp");
        std::fs::write(&tmp, "a = 2\n").unwrap();
        std::fs::OpenOptions::new().write(true).open(&tmp).unwrap().set_modified(modified).unwrap();
        std::fs::rename(&tmp, &path).unwrap();
        assert_ne!(Stamp::of(&path), Some(before));
    }
}
