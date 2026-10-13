//! Reading from the git repository a file lives in, through the `git` CLI.
//! Calls block on a child process: run them off the UI thread.

use std::path::{Path, PathBuf};
use std::process::Command;

/// The text of `path` as committed at `HEAD`, or `None` if the file isn't in a
/// repository, isn't tracked, or git isn't available.
pub fn head_text(path: &Path) -> Option<String> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy();
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .arg("show")
        .arg(format!("HEAD:./{name}"))
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The root of the repository `path` lives in: the nearest ancestor (or `path`
/// itself, if a directory) holding a `.git` entry. Looks at the filesystem
/// only, so it is cheap.
pub fn repo_root(path: &Path) -> Option<PathBuf> {
    let start = if path.is_dir() { path } else { path.parent()? };
    start.ancestors().find(|d| d.join(".git").exists()).map(Path::to_path_buf)
}

/// Whether everything at `path` is tracked by git and unmodified, so a copy
/// survives in the repository (a file: tracked and clean; a directory: it
/// holds tracked files and nothing untracked, ignored, or changed). `false`
/// outside a repository or when git isn't available.
pub fn is_tracked_clean(path: &Path) -> bool {
    let (Some(dir), Some(name)) = (
        if path.is_dir() { Some(path) } else { path.parent() },
        if path.is_dir() { Some(Path::new(".")) } else { path.file_name().map(Path::new) },
    ) else {
        return false;
    };
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .arg("--")
            .arg(name)
            .output()
            .ok()
            .filter(|o| o.status.success())
    };
    let tracked = git(&["ls-files", "-z"]).is_some_and(|o| !o.stdout.is_empty());
    tracked && git(&["status", "--porcelain", "--ignored"]).is_some_and(|o| o.stdout.is_empty())
}

/// The repository's `info/exclude` file (it may not exist yet). Asks git, as
/// the git directory isn't always `.git` (worktrees, submodules).
pub fn exclude_file(root: &Path) -> Option<PathBuf> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "--git-path", "info/exclude"])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let path = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim_end());
    Some(if path.is_absolute() { path } else { root.join(path) })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git").arg("-C").arg(dir).args(args).output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn reads_the_committed_text_of_tracked_files_only() {
        let dir = std::env::temp_dir().join(format!("ghostrealm-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        if !git(&dir, &["init", "-q"]) {
            return; // no git here
        }
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        assert!(git(&dir, &["add", "a.txt"]));
        let ok = git(
            &dir,
            &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "init"],
        );
        assert!(ok);
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("b.txt"), "new\n").unwrap();
        assert_eq!(head_text(&dir.join("a.txt")).as_deref(), Some("one\n"), "HEAD, not the worktree");
        assert_eq!(head_text(&dir.join("b.txt")), None, "untracked");
        assert_eq!(head_text(Path::new("/no/such/dir/x.txt")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn init_repo(tag: &str) -> Option<PathBuf> {
        let dir = std::env::temp_dir().join(format!("ghostrealm-git-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git(&dir, &["init", "-q"]).then_some(dir)
    }

    fn commit_all(dir: &Path) {
        assert!(git(dir, &["add", "-A"]));
        assert!(git(
            dir,
            &["-c", "user.name=t", "-c", "user.email=t@t", "-c", "commit.gpgsign=false", "commit", "-qm", "c"]
        ));
    }

    #[test]
    fn tracked_and_unmodified_paths_are_recoverable_and_others_are_not() {
        let Some(dir) = init_repo("clean") else { return };
        std::fs::create_dir(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "b\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
        commit_all(&dir);
        assert!(is_tracked_clean(&dir.join("a.txt")));
        assert!(is_tracked_clean(&dir.join("sub")));
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        assert!(!is_tracked_clean(&dir.join("a.txt")), "modified");
        std::fs::write(dir.join("new.txt"), "n").unwrap();
        assert!(!is_tracked_clean(&dir.join("new.txt")), "untracked");
        std::fs::write(dir.join("sub/extra.txt"), "x").unwrap();
        assert!(!is_tracked_clean(&dir.join("sub")), "holds an untracked file");
        std::fs::write(dir.join("ignored.log"), "x").unwrap();
        assert!(!is_tracked_clean(&dir.join("ignored.log")), "ignored files aren't in the repository");
        assert!(!is_tracked_clean(Path::new("/no/such/file")));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_repo_root_and_exclude_file_are_found_from_inside() {
        let Some(dir) = init_repo("root") else { return };
        std::fs::create_dir_all(dir.join("a/b")).unwrap();
        std::fs::write(dir.join("a/b/f.txt"), "x").unwrap();
        assert_eq!(repo_root(&dir.join("a/b/f.txt")), Some(dir.clone()));
        assert_eq!(repo_root(&dir.join("a/b")), Some(dir.clone()));
        let exclude = exclude_file(&dir).unwrap();
        assert!(exclude.ends_with(".git/info/exclude"), "{exclude:?}");
        assert_eq!(repo_root(Path::new("/")), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
