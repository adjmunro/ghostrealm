//! What identifies one version of a file on disk, for noticing that another
//! program wrote it: modification time and size.

use std::path::Path;
use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    /// The file at `path` as it is now; `None` while it is absent.
    pub fn of(path: &Path) -> Option<Stamp> {
        let meta = std::fs::metadata(path).ok()?;
        Some(Stamp { modified: meta.modified().ok(), len: meta.len() })
    }
}
