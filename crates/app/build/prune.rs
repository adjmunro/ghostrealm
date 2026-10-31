//! Pruning `target/` after a build. Cargo has no post-build hook and holds
//! the target dir's locks while this script runs, so the script starts
//! `scripts/clean-stale.rs` detached, with `--wait`: it cleans once the build
//! releases them. At most once per `EVERY`, never with `CARGO_TARGET_DIR`
//! (clean-stale refuses a target dir other workspaces may share), and never
//! when `OPT_OUT` is set (not empty or `0`).

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub const OPT_OUT: &str = "GHOSTREALM_NO_PRUNE";
pub const EVERY: Duration = Duration::from_secs(3600);
/// In the target dir; its mtime is the last prune's start.
pub const STAMP: &str = "clean-stale-after-build.stamp";
/// The pruner binary and, with `.log`, its output, in the target dir.
pub const BIN: &str = "clean-stale-after-build";
/// Longest wait for the build to finish, in seconds.
pub const WAIT: &str = "1800";

/// The checkout's own `target/` when the build that set `out_dir` uses it.
pub fn own_target(root: &Path, out_dir: &Path, cargo_target_dir: Option<&str>) -> Option<PathBuf> {
    if cargo_target_dir.is_some() {
        return None;
    }
    let target = fs::canonicalize(root.join("target")).ok()?;
    fs::canonicalize(out_dir).ok()?.starts_with(&target).then_some(target)
}

/// Whether to prune now: not opted out, and `stamp` missing or older than
/// `EVERY`. Restamps when it says yes; a stamp it can't write means no.
pub fn due(opt_out: Option<&str>, stamp: &Path, now: SystemTime) -> bool {
    if opt_out.is_some_and(|v| !v.is_empty() && v != "0") {
        return false;
    }
    let last = fs::metadata(stamp).and_then(|m| m.modified());
    if last.is_ok_and(|t| now.duration_since(t).is_ok_and(|age| age < EVERY)) {
        return false;
    }
    File::create(stamp).and_then(|f| f.set_modified(now)).is_ok()
}
