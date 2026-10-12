//! Reading from the git repository a file lives in, through the `git` CLI.
//! Calls block on a child process: run them off the UI thread.

use std::path::Path;
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
}
