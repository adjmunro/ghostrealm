//! A `rerun-if-changed` trigger for files that don't exist yet. Cargo reruns
//! a build script on every build while a watched path is missing, and it
//! watches a directory with everything below it. So the trigger is a
//! directory of symlinks to the missing files: Cargo skips a symlink it
//! can't follow, and once its target appears, the target's mtime (newer than
//! the run) reruns the script. Dependency-free, so a test can build it into
//! a scratch crate and check that Cargo behaves so.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The name of the link to `target` in the trigger dir.
fn link_name(target: &Path) -> String {
    target.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() || c == '.' { c } else { '-' }).collect()
}

/// Make `dir` hold one symlink per `absent` path, named after it, and
/// nothing else, then put `dir`'s mtime at `at` (before the run's start) if
/// it is later. Only symlinks are removed: any other entry is an error, and
/// kept.
#[cfg(unix)]
pub fn watch_absent(dir: &Path, absent: &[PathBuf], at: SystemTime) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let want: Vec<(String, &PathBuf)> = absent.iter().map(|p| (link_name(p), p)).collect();
    for e in fs::read_dir(dir)? {
        let e = e?;
        let name = e.file_name().to_string_lossy().into_owned();
        let link = e.path();
        if !e.file_type()?.is_symlink() {
            return Err(io::Error::other(format!("{}: not a symlink; remove it by hand", link.display())));
        }
        if !want.iter().any(|(n, p)| *n == name && fs::read_link(&link).is_ok_and(|t| t == **p)) {
            fs::remove_file(&link)?;
        }
    }
    for (name, target) in want {
        let link = dir.join(name);
        if fs::symlink_metadata(&link).is_err() {
            std::os::unix::fs::symlink(target, &link)?;
        }
    }
    if fs::metadata(dir)?.modified()? > at {
        fs::File::open(dir)?.set_modified(at)?;
    }
    Ok(())
}
