//! Filesystem work behind the file and folder context menu: creating,
//! copying, moving and renaming entries, ignore-file edits, and the calls out
//! to the system (Trash, the file manager, the default app) behind [`System`]
//! so tests never touch the real ones.

use std::ffi::OsStr;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use ghostrealm_core::git;

/// What the app asks of the operating system outside its own windows.
pub trait System {
    /// Move `path` to the Trash. Never deletes permanently.
    fn trash(&self, path: &Path) -> Result<()>;
    /// Show `path` selected in the system file manager.
    fn reveal(&self, path: &Path) -> Result<()>;
    /// Open `path` with the system's default application.
    fn open_default(&self, path: &Path) -> Result<()>;
}

/// The real thing.
pub struct HostSystem;

impl System for HostSystem {
    #[cfg(target_os = "macos")]
    fn trash(&self, path: &Path) -> Result<()> {
        use objc2_foundation::{NSFileManager, NSString, NSURL};
        let path = path.to_str().context("the path isn't UTF-8")?;
        let url = NSURL::fileURLWithPath(&NSString::from_str(path));
        NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .map_err(|e| anyhow::anyhow!("{}", e.localizedDescription()))
    }

    #[cfg(not(target_os = "macos"))]
    fn trash(&self, _path: &Path) -> Result<()> {
        bail!("moving to the Trash isn't supported on this platform")
    }

    fn reveal(&self, path: &Path) -> Result<()> {
        if cfg!(target_os = "macos") {
            run("open", [OsStr::new("-R"), path.as_os_str()])
        } else {
            run("xdg-open", [path.parent().unwrap_or(path).as_os_str()])
        }
    }

    fn open_default(&self, path: &Path) -> Result<()> {
        run(if cfg!(target_os = "macos") { "open" } else { "xdg-open" }, [path.as_os_str()])
    }
}

fn run<'a>(program: &str, args: impl IntoIterator<Item = &'a OsStr>) -> Result<()> {
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("can't run `{program}`"))?;
    if !status.success() {
        bail!("`{program}` failed ({status})");
    }
    Ok(())
}

/// A name for a new entry: it must be a single, plain path component.
fn check_name(name: &str) -> Result<&str> {
    let name = name.trim();
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(_)), None) => Ok(name),
        _ => bail!("\u{201c}{name}\u{201d} isn't a valid name"),
    }
}

/// Create the empty file `name` in `dir`. Fails if something is already there.
pub fn create_file(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(check_name(name)?);
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
        .with_context(|| format!("can't create {}", path.display()))?;
    Ok(path)
}

/// Create the directory `name` in `dir`. Fails if something is already there.
pub fn create_dir(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(check_name(name)?);
    std::fs::create_dir(&path).with_context(|| format!("can't create {}", path.display()))?;
    Ok(path)
}

/// `name` split into a stem and a dotted extension (`"a.txt"` -> `("a", ".txt")`;
/// a dotfile has none), with any " copy" / " copy N" suffix already on the stem
/// removed.
fn split_copy_name(name: &str) -> (String, String) {
    let path = Path::new(name);
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let base = match stem.rsplit_once(" copy ") {
        Some((base, n)) if !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) => base,
        _ => stem.strip_suffix(" copy").unwrap_or(&stem),
    };
    (base.to_string(), ext)
}

/// Where a copy of `name` goes in `dir`: the name itself when free, else
/// Finder's pattern (`a copy.txt`, `a copy 2.txt`, ...).
pub fn unique_dest(dir: &Path, name: &OsStr) -> PathBuf {
    let plain = dir.join(name);
    if plain.symlink_metadata().is_err() {
        return plain;
    }
    let (base, ext) = split_copy_name(&name.to_string_lossy());
    (1..)
        .map(|n| match n {
            1 => dir.join(format!("{base} copy{ext}")),
            n => dir.join(format!("{base} copy {n}{ext}")),
        })
        .find(|p| p.symlink_metadata().is_err())
        .expect("an unused name")
}

/// Create `dst` as a copy of the entry `src`: a file with its contents, a link,
/// or an empty directory (its contents are [`copy_children`]'s). Fails with
/// `AlreadyExists` rather than replacing whatever is at `dst`.
fn create_copy(src: &Path, dst: &Path) -> Result<()> {
    let meta = src.symlink_metadata().with_context(|| format!("can't read {}", src.display()))?;
    if meta.is_dir() {
        std::fs::create_dir(dst)?;
    } else if meta.is_symlink() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
        #[cfg(not(unix))]
        std::fs::copy(src, dst)?;
    } else if meta.is_file() {
        let mut from = std::fs::File::open(src)?;
        let mut to = std::fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
        std::io::copy(&mut from, &mut to)?;
        to.set_permissions(from.metadata()?.permissions())?;
    } else {
        // A pipe, socket, or device: opening one to read it can block forever.
        bail!("can't copy {}: not a regular file", src.display());
    }
    Ok(())
}

/// Copy what is inside the directory `src` into the existing directory `dst`.
fn copy_children(src: &Path, dst: &Path) -> Result<()> {
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
    }
    Ok(())
}

/// Copy `src` (a file, or a directory with its contents) to `dst`.
fn copy_recursive(src: &Path, dst: &Path) -> Result<()> {
    create_copy(src, dst)?;
    if src.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        copy_children(src, dst)?;
    }
    Ok(())
}

/// Whether `e` is (or wraps) an `AlreadyExists` I/O error.
fn is_taken(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == ErrorKind::AlreadyExists)
}

/// Copy `src` to a free name in `dir`. A name another process takes between
/// the check and the copy is skipped, never overwritten.
fn copy_to_free(src: &Path, dir: &Path, name: &OsStr) -> Result<PathBuf> {
    loop {
        let dest = unique_dest(dir, name);
        match create_copy(src, &dest) {
            Ok(()) => {
                if src.symlink_metadata().is_ok_and(|m| m.is_dir()) {
                    copy_children(src, &dest)?;
                }
                return Ok(dest);
            }
            Err(e) if is_taken(&e) => continue,
            Err(e) => return Err(e),
        }
    }
}

/// Rename `from` to `to` unless something is already at `to`, in which case it
/// fails with `AlreadyExists` and changes nothing. `std::fs::rename` would
/// replace a file or an empty directory that appeared since the caller looked.
/// Where the system has no atomic form of this, the check and the rename are
/// separate steps.
fn rename_noreplace(from: &Path, to: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = |p: &Path| {
            std::ffi::CString::new(p.as_os_str().as_bytes())
                .map_err(|_| std::io::Error::from(ErrorKind::InvalidInput))
        };
        let (f, t) = (c(from)?, c(to)?);
        #[cfg(target_os = "macos")]
        // SAFETY: both are valid NUL-terminated strings for the call's duration.
        let rc = unsafe { libc::renamex_np(f.as_ptr(), t.as_ptr(), libc::RENAME_EXCL) };
        #[cfg(target_os = "linux")]
        // SAFETY: as above.
        let rc = unsafe {
            libc::renameat2(libc::AT_FDCWD, f.as_ptr(), libc::AT_FDCWD, t.as_ptr(), libc::RENAME_NOREPLACE)
        };
        if rc == 0 {
            return Ok(());
        }
        let err = std::io::Error::last_os_error();
        // This filesystem or kernel can't do it: fall through to the plain way.
        if !matches!(err.raw_os_error(), Some(libc::EINVAL | libc::ENOTSUP | libc::ENOSYS)) {
            return Err(err);
        }
    }
    if to.symlink_metadata().is_ok() {
        return Err(ErrorKind::AlreadyExists.into());
    }
    std::fs::rename(from, to)
}

/// Whether `dir` is `src` or inside it, after resolving symlinks (`src`'s
/// own last component isn't followed: a symlink is copied or moved as a link).
/// Copying a folder into itself would recurse until the stack overflows.
fn inside_itself(src: &Path, dir: &Path) -> bool {
    let real = |p: &Path| p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    let src = match (src.parent(), src.file_name()) {
        (Some(parent), Some(name)) => real(parent).join(name),
        _ => real(src),
    };
    real(dir).starts_with(src)
}

/// Copy `src` into `dir` under a free name. Returns the copy's path.
pub fn copy_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let name = src.file_name().context("nothing to copy")?;
    if inside_itself(src, dir) {
        bail!("can't copy a folder into itself");
    }
    copy_to_free(src, dir, name).with_context(|| format!("can't copy {}", src.display()))
}

/// Copy `src` next to itself. Returns the copy's path.
pub fn duplicate(src: &Path) -> Result<PathBuf> {
    copy_into(src, src.parent().context("nothing to duplicate")?)
}

/// Move `src` into `dir` under a free name. Moving into the folder it is
/// already in changes nothing. Returns the new path.
pub fn move_into(src: &Path, dir: &Path) -> Result<PathBuf> {
    let name = src.file_name().context("nothing to move")?;
    if src.parent() == Some(dir) {
        return Ok(src.to_path_buf());
    }
    if inside_itself(src, dir) {
        bail!("can't move a folder into itself");
    }
    loop {
        let dest = unique_dest(dir, name);
        match rename_noreplace(src, &dest) {
            Ok(()) => return Ok(dest),
            // Another process took the name since it was picked: pick again.
            Err(e) if e.kind() == ErrorKind::AlreadyExists => continue,
            // Across filesystems a rename isn't possible: copy, then remove.
            Err(e) if e.kind() == ErrorKind::CrossesDevices => {
                let dest = copy_to_free(src, dir, name)?;
                if src.is_dir() {
                    std::fs::remove_dir_all(src)?;
                } else {
                    std::fs::remove_file(src)?;
                }
                return Ok(dest);
            }
            Err(e) => return Err(e).with_context(|| format!("can't move {}", src.display())),
        }
    }
}

/// Rename `path` to `name` within its folder. Fails if the name is taken.
pub fn rename(path: &Path, name: &str) -> Result<PathBuf> {
    let dest = path.with_file_name(check_name(name)?);
    if dest == path {
        return Ok(dest);
    }
    let taken = || anyhow::anyhow!("\u{201c}{name}\u{201d} already exists");
    // The same entry under a name that differs in case is not "taken", and a
    // no-replace rename would refuse it.
    let result = if same_entry_recased(path, &dest) {
        std::fs::rename(path, &dest)
    } else if dest.symlink_metadata().is_ok() {
        return Err(taken());
    } else {
        rename_noreplace(path, &dest)
    };
    match result {
        Ok(()) => Ok(dest),
        Err(e) if e.kind() == ErrorKind::AlreadyExists => Err(taken()),
        Err(e) => Err(e).with_context(|| format!("can't rename {}", path.display())),
    }
}

/// Whether `dest` names the entry `path` with only the case changed, as a
/// case-insensitive filesystem (macOS's default) finds it already there.
fn same_entry_recased(path: &Path, dest: &Path) -> bool {
    let same_name = match (path.file_name(), dest.file_name()) {
        (Some(a), Some(b)) => a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase(),
        _ => false,
    };
    #[cfg(unix)]
    let same_file = {
        use std::os::unix::fs::MetadataExt;
        match (path.symlink_metadata(), dest.symlink_metadata()) {
            (Ok(a), Ok(b)) => (a.dev(), a.ino()) == (b.dev(), b.ino()),
            _ => false,
        }
    };
    #[cfg(not(unix))]
    let same_file = false;
    same_name && same_file
}

/// Where `path` is now, given that each `(from, to)` of `moved` was moved:
/// `to` itself when `path` is `from`, or the same place inside `to`. `None`
/// when none of them held it.
pub fn follow(path: &Path, moved: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    moved.iter().find_map(|(from, to)| {
        let rest = path.strip_prefix(from).ok()?;
        Some(if rest.as_os_str().is_empty() { to.clone() } else { to.join(rest) })
    })
}

/// `path` relative to `root`, with `/` separators. A path outside `root` is
/// returned whole.
pub fn relative_to(path: &Path, root: &Path) -> String {
    let Ok(rel) = path.strip_prefix(root) else {
        return path.to_string_lossy().into_owned();
    };
    rel.components().map(|c| c.as_os_str().to_string_lossy()).collect::<Vec<_>>().join("/")
}

/// The pattern that ignores `path` in the repository at `root`: anchored at the
/// root (`/src/a.rs`), with a trailing `/` for a directory (not a symlink to
/// one: git sees that as an entry of its own). Glob characters, backslashes,
/// and trailing spaces in names are escaped, so the line matches that entry
/// only; a leading `#` or `!` needs nothing, as the `/` comes first.
pub fn ignore_pattern(path: &Path, root: &Path) -> String {
    let mut pattern = String::from("/");
    for c in relative_to(path, root).chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            pattern.push('\\');
        }
        pattern.push(c);
    }
    let spaces = pattern.len() - pattern.trim_end_matches(' ').len();
    pattern.truncate(pattern.len() - spaces);
    pattern.push_str(&"\\ ".repeat(spaces));
    if path.symlink_metadata().is_ok_and(|m| m.is_dir()) {
        pattern.push('/');
    }
    pattern
}

/// Add `pattern` as a line of the ignore file `file`, creating it (and its
/// folder) if needed. A line that already ignores the same thing (with or
/// without the leading `/`) is left alone. Returns whether the file changed.
pub fn add_ignore_line(file: &Path, pattern: &str) -> Result<bool> {
    let existing = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("can't read {}", file.display())),
    };
    // Trailing whitespace compares trimmed on both sides: git drops it from a
    // line unless escaped, and an escaped one ends in `\`.
    let bare = pattern.trim_start_matches('/').trim_end();
    if existing.lines().any(|l| l.trim().trim_start_matches('/') == bare) {
        return Ok(false);
    }
    let mut text = existing;
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(pattern);
    text.push('\n');
    if let Some(dir) = file.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(file, text).with_context(|| format!("can't write {}", file.display()))?;
    Ok(true)
}

/// The repository whose index would hold `path`: the one around its folder
/// (a folder that is a repository of its own is still an entry of the one
/// it sits in).
pub fn containing_repo(path: &Path) -> Result<PathBuf> {
    path.parent().and_then(git::repo_root).context("not in a git repository")
}

/// Ignore `path` in its repository's `.gitignore`. Returns whether it changed.
pub fn add_to_gitignore(path: &Path) -> Result<bool> {
    let root = containing_repo(path)?;
    add_ignore_line(&root.join(".gitignore"), &ignore_pattern(path, &root))
}

/// Ignore `path` in its repository's `.git/info/exclude`. Returns whether it
/// changed.
pub fn add_to_exclude(path: &Path) -> Result<bool> {
    let root = containing_repo(path)?;
    let exclude = git::exclude_file(&root).context("can't locate .git/info/exclude")?;
    add_ignore_line(&exclude, &ignore_pattern(path, &root))
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A [`System`] that records what it was asked (in `calls`, which the test
    /// keeps a handle on) and touches nothing but the entry it "trashes".
    #[derive(Default)]
    pub struct FakeSystem {
        pub calls: Rc<RefCell<Vec<String>>>,
        pub fail_trash: bool,
    }

    impl System for FakeSystem {
        fn trash(&self, path: &Path) -> Result<()> {
            if self.fail_trash {
                bail!("no Trash here");
            }
            self.calls.borrow_mut().push(format!("trash {}", path.display()));
            // Stand in for the Trash: the entry leaves its place.
            if path.is_dir() {
                std::fs::remove_dir_all(path)?;
            } else {
                std::fs::remove_file(path)?;
            }
            Ok(())
        }

        fn reveal(&self, path: &Path) -> Result<()> {
            self.calls.borrow_mut().push(format!("reveal {}", path.display()));
            Ok(())
        }

        fn open_default(&self, path: &Path) -> Result<()> {
            self.calls.borrow_mut().push(format!("open {}", path.display()));
            Ok(())
        }
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    #[test]
    fn new_files_and_folders_are_created_once_with_plain_names() {
        let d = tmpdir("ops-new");
        assert_eq!(create_file(&d, "a.txt").unwrap(), d.join("a.txt"));
        assert_eq!(create_dir(&d, "sub").unwrap(), d.join("sub"));
        assert!(create_file(&d, "a.txt").is_err(), "exists");
        assert!(create_dir(&d, "sub").is_err(), "exists");
        for bad in ["", "  ", "a/b", "..", "."] {
            assert!(create_file(&d, bad).is_err(), "{bad:?}");
        }
        assert_eq!(names(&d), ["a.txt", "sub"]);
    }

    #[test]
    fn copies_are_named_like_finders() {
        let d = tmpdir("ops-dup");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join(".env"), "E").unwrap();
        std::fs::create_dir(d.join("dir")).unwrap();
        std::fs::write(d.join("dir/in.txt"), "I").unwrap();
        assert_eq!(duplicate(&d.join("a.txt")).unwrap(), d.join("a copy.txt"));
        assert_eq!(duplicate(&d.join("a.txt")).unwrap(), d.join("a copy 2.txt"));
        assert_eq!(duplicate(&d.join("a copy.txt")).unwrap(), d.join("a copy 3.txt"), "no 'copy copy'");
        assert_eq!(duplicate(&d.join(".env")).unwrap(), d.join(".env copy"));
        let dir_copy = duplicate(&d.join("dir")).unwrap();
        assert_eq!(dir_copy, d.join("dir copy"));
        assert_eq!(std::fs::read_to_string(dir_copy.join("in.txt")).unwrap(), "I", "contents come along");
        assert_eq!(std::fs::read_to_string(d.join("a copy 2.txt")).unwrap(), "A");
    }

    #[test]
    fn copy_pastes_into_another_folder_with_its_name_unless_taken() {
        let d = tmpdir("ops-paste");
        std::fs::create_dir(d.join("to")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        assert_eq!(copy_into(&d.join("a.txt"), &d.join("to")).unwrap(), d.join("to/a.txt"));
        assert_eq!(copy_into(&d.join("a.txt"), &d.join("to")).unwrap(), d.join("to/a copy.txt"));
        assert!(d.join("a.txt").exists(), "a copy leaves the original");
        assert!(copy_into(&d.join("to"), &d.join("to")).is_err(), "not into itself");
    }

    #[test]
    fn cut_pastes_move_the_entry() {
        let d = tmpdir("ops-move");
        std::fs::create_dir_all(d.join("to/deep")).unwrap();
        std::fs::write(d.join("a.txt"), "A").unwrap();
        assert_eq!(move_into(&d.join("a.txt"), &d.join("to")).unwrap(), d.join("to/a.txt"));
        assert!(!d.join("a.txt").exists());
        assert_eq!(move_into(&d.join("to/a.txt"), &d.join("to")).unwrap(), d.join("to/a.txt"), "same folder: no-op");
        assert!(move_into(&d.join("to"), &d.join("to/deep")).is_err(), "not into itself");
        std::fs::write(d.join("a.txt"), "B").unwrap();
        assert_eq!(move_into(&d.join("a.txt"), &d.join("to")).unwrap(), d.join("to/a copy.txt"), "a taken name");
        assert_eq!(std::fs::read_to_string(d.join("to/a.txt")).unwrap(), "A", "nothing is overwritten");
    }

    #[test]
    fn rename_keeps_the_folder_and_refuses_taken_or_nested_names() {
        let d = tmpdir("ops-rename");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        std::fs::write(d.join("b.txt"), "B").unwrap();
        assert_eq!(rename(&d.join("a.txt"), "c.txt").unwrap(), d.join("c.txt"));
        assert!(rename(&d.join("c.txt"), "b.txt").is_err(), "taken");
        assert!(rename(&d.join("c.txt"), "x/y.txt").is_err(), "nested");
        assert_eq!(rename(&d.join("c.txt"), "c.txt").unwrap(), d.join("c.txt"), "unchanged");
        assert_eq!(names(&d), ["b.txt", "c.txt"]);
    }

    #[test]
    fn moved_paths_are_followed_without_a_trailing_slash() {
        let moved = [(PathBuf::from("/w/a"), PathBuf::from("/w/b"))];
        assert_eq!(follow(Path::new("/w/a"), &moved), Some(PathBuf::from("/w/b")));
        assert_eq!(follow(Path::new("/w/a/x/y.rs"), &moved), Some(PathBuf::from("/w/b/x/y.rs")));
        assert_eq!(follow(Path::new("/w/ab"), &moved), None);
        assert_eq!(follow(Path::new("/w/a"), &[]), None);
    }

    #[test]
    fn relative_paths_use_slashes_from_the_root() {
        let root = Path::new("/w/proj");
        assert_eq!(relative_to(Path::new("/w/proj/src/a.rs"), root), "src/a.rs");
        assert_eq!(relative_to(Path::new("/elsewhere/a.rs"), root), "/elsewhere/a.rs");
    }

    fn git_ok(dir: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git").arg("-C").arg(dir).args(args).output().is_ok_and(|o| o.status.success())
    }

    #[test]
    fn ignore_lines_are_anchored_appended_and_never_duplicated() {
        let d = tmpdir("ops-ignore");
        if !git_ok(&d, &["init", "-q"]) {
            return;
        }
        std::fs::create_dir_all(d.join("build/out")).unwrap();
        std::fs::write(d.join("build/out/x.o"), "").unwrap();
        std::fs::write(d.join("notes.txt"), "").unwrap();
        // A file, then a directory (trailing slash), in .gitignore (no trailing newline yet).
        std::fs::write(d.join(".gitignore"), "target").unwrap();
        assert!(add_to_gitignore(&d.join("notes.txt")).unwrap());
        assert!(add_to_gitignore(&d.join("build")).unwrap());
        assert!(add_to_gitignore(&d.join("build/out/x.o")).unwrap());
        assert_eq!(
            std::fs::read_to_string(d.join(".gitignore")).unwrap(),
            "target\n/notes.txt\n/build/\n/build/out/x.o\n"
        );
        assert!(!add_to_gitignore(&d.join("notes.txt")).unwrap(), "already there");
        assert!(!add_to_gitignore(&d.join("build")).unwrap(), "already there");
        std::fs::write(d.join(".gitignore"), "notes.txt\n").unwrap();
        assert!(!add_to_gitignore(&d.join("notes.txt")).unwrap(), "unanchored line covers it");
        assert_eq!(std::fs::read_to_string(d.join(".gitignore")).unwrap(), "notes.txt\n");
        // The exclude file is created on demand and takes the same lines.
        assert!(add_to_exclude(&d.join("notes.txt")).unwrap());
        assert!(!add_to_exclude(&d.join("notes.txt")).unwrap());
        assert_eq!(std::fs::read_to_string(d.join(".git/info/exclude")).unwrap().lines().last(), Some("/notes.txt"));
        // Outside a repository there is nothing to edit.
        let loose = tmpdir("ops-ignore-loose");
        std::fs::write(loose.join("f"), "").unwrap();
        if git::repo_root(&loose).is_none() {
            assert!(add_to_gitignore(&loose.join("f")).is_err());
        }
    }

    #[test]
    fn an_ignore_line_matches_only_its_own_entry_whatever_its_name() {
        let d = tmpdir("ops-ignore-names");
        if !git_ok(&d, &["init", "-q"]) {
            return;
        }
        let ignored = |name: &str| git_ok(&d, &["check-ignore", "-q", "--no-index", "--", name]);
        for name in ["*.rs", "a?", "[ab]", "back\\slash", "trail ", "#hash", "!bang"] {
            std::fs::write(d.join(name), "").unwrap();
        }
        for other in ["x.rs", "ab", "a", "b", "trail"] {
            std::fs::write(d.join(other), "").unwrap();
        }
        for name in ["*.rs", "a?", "[ab]", "back\\slash", "trail ", "#hash", "!bang"] {
            assert!(add_to_gitignore(&d.join(name)).unwrap(), "{name:?}");
            assert!(ignored(name), "{name:?} is ignored");
            assert!(!add_to_gitignore(&d.join(name)).unwrap(), "{name:?} is already there");
        }
        for other in ["x.rs", "ab", "a", "b", "trail"] {
            assert!(!ignored(other), "{other:?} isn't caught by another entry's line");
        }
        // A symlink to a folder is an entry of its own to git, not a folder.
        std::fs::create_dir(d.join("real")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("real", d.join("link")).unwrap();
            add_to_gitignore(&d.join("link")).unwrap();
            assert!(ignored("link"));
        }
        // A repository nested in this one is ignored here, as a folder.
        std::fs::create_dir(d.join("nested")).unwrap();
        assert!(git_ok(&d.join("nested"), &["init", "-q"]));
        assert!(add_to_gitignore(&d.join("nested")).unwrap());
        assert!(ignored("nested"));
        assert!(!d.join("nested/.gitignore").exists(), "not the nested repository's own file");
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_isnt_copied_or_moved_into_itself_through_a_symlink() {
        let d = tmpdir("ops-into-self");
        std::fs::create_dir_all(d.join("a/sub")).unwrap();
        std::fs::write(d.join("a/f.txt"), "F").unwrap();
        std::os::unix::fs::symlink(d.join("a/sub"), d.join("link")).unwrap();
        assert!(copy_into(&d.join("a"), &d.join("link")).is_err());
        assert!(move_into(&d.join("a"), &d.join("link")).is_err());
        assert_eq!(names(&d.join("a/sub")), Vec::<String>::new(), "nothing was written");
        // A symlink itself is copied as a link, even into the folder it names.
        assert_eq!(copy_into(&d.join("link"), &d.join("a/sub")).unwrap(), d.join("a/sub/link"));
    }

    #[cfg(unix)]
    #[test]
    fn a_folder_holding_a_pipe_fails_to_copy_instead_of_waiting_on_it() {
        let d = tmpdir("ops-fifo");
        std::fs::create_dir(d.join("dir")).unwrap();
        if !std::process::Command::new("mkfifo").arg(d.join("dir/pipe")).status().is_ok_and(|s| s.success()) {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let src = d.join("dir");
        std::thread::spawn(move || tx.send(duplicate(&src).is_err()));
        let failed = rx.recv_timeout(std::time::Duration::from_secs(5)).expect("the copy returns");
        assert!(failed);
    }

    #[test]
    fn a_rename_may_change_only_the_case_of_a_name() {
        let d = tmpdir("ops-rename-case");
        std::fs::write(d.join("a.txt"), "A").unwrap();
        assert_eq!(rename(&d.join("a.txt"), "A.txt").unwrap(), d.join("A.txt"));
        assert_eq!(names(&d), ["A.txt"]);
        assert_eq!(std::fs::read_to_string(d.join("A.txt")).unwrap(), "A");
        // Another file whose name differs only in case is still taken.
        std::fs::write(d.join("b.txt"), "B").unwrap();
        if d.join("B.TXT").exists() {
            assert!(rename(&d.join("A.txt"), "B.txt").is_err(), "case-insensitive: b.txt is taken");
            assert_eq!(std::fs::read_to_string(d.join("b.txt")).unwrap(), "B");
        }
    }

    #[test]
    fn a_no_replace_rename_leaves_what_is_already_at_the_target() {
        let d = tmpdir("ops-noreplace");
        std::fs::write(d.join("a"), "A").unwrap();
        std::fs::write(d.join("b"), "B").unwrap();
        std::fs::create_dir(d.join("empty")).unwrap();
        std::fs::create_dir(d.join("src")).unwrap();
        let err = rename_noreplace(&d.join("a"), &d.join("b")).unwrap_err();
        assert_eq!(err.kind(), ErrorKind::AlreadyExists, "a file");
        assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "B");
        assert_eq!(rename_noreplace(&d.join("src"), &d.join("empty")).unwrap_err().kind(), ErrorKind::AlreadyExists);
        assert!(d.join("src").is_dir(), "an empty folder isn't replaced either");
        rename_noreplace(&d.join("a"), &d.join("c")).unwrap();
        assert_eq!(names(&d), ["b", "c", "empty", "src"]);
    }

    #[test]
    fn copies_never_replace_what_is_at_the_target() {
        let d = tmpdir("ops-copy-excl");
        std::fs::create_dir(d.join("dir")).unwrap();
        std::fs::write(d.join("a"), "A").unwrap();
        std::fs::write(d.join("b"), "B").unwrap();
        assert!(copy_recursive(&d.join("a"), &d.join("b")).is_err());
        assert_eq!(std::fs::read_to_string(d.join("b")).unwrap(), "B");
        assert!(copy_recursive(&d.join("dir"), &d.join("dir")).is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("a", d.join("link")).unwrap();
            assert!(copy_recursive(&d.join("link"), &d.join("b")).is_err());
        }
        // Permissions come along with the contents.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(d.join("a"), std::fs::Permissions::from_mode(0o750)).unwrap();
            copy_recursive(&d.join("a"), &d.join("a2")).unwrap();
            assert_eq!(std::fs::metadata(d.join("a2")).unwrap().permissions().mode() & 0o777, 0o750);
        }
    }
}
