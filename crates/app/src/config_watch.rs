//! Notices edits to the config files made outside the app (another editor, a
//! synced dotfile) by comparing its modification time and size. No file-watch
//! dependency: the window polls on a slow timer while focused and when it
//! gains focus.

use std::path::PathBuf;
use std::time::Duration;

use crate::file_stamp::Stamp;

/// How often the focused window checks the config file.
pub const POLL: Duration = Duration::from_secs(2);

pub struct ConfigWatch {
    files: Vec<Watched>,
}

struct Watched {
    path: PathBuf,
    /// The version last loaded or declined; `None` while the file is absent.
    seen: Option<Stamp>,
    /// Removing the file is a change (it only overrides another).
    optional: bool,
}

impl ConfigWatch {
    /// Watch `path`, taking the file as it is now to be the version loaded.
    pub fn new(path: Option<PathBuf>) -> Self {
        let files = path.map(|path| Watched { path, seen: None, optional: false });
        let mut w = ConfigWatch { files: files.into_iter().collect() };
        w.mark_seen();
        w
    }

    /// Also watch `path`, a file laid over the first, whose removal is a
    /// change.
    pub fn with_overlay(mut self, path: Option<PathBuf>) -> Self {
        if let Some(path) = path {
            let seen = Stamp::of(&path);
            self.files.push(Watched { path, seen, optional: true });
        }
        self
    }

    /// The files as they are now are the versions the running config came
    /// from (the app just loaded or wrote them).
    pub fn mark_seen(&mut self) {
        for f in &mut self.files {
            f.seen = Stamp::of(&f.path);
        }
    }

    /// Whether a file changed since the last version seen. The main file
    /// gone missing is not a change (the running config stays); its return
    /// is. An overlay's removal is.
    pub fn changed(&mut self) -> bool {
        let mut changed = false;
        for f in &mut self.files {
            let now = Stamp::of(&f.path);
            if now != f.seen {
                changed |= now.is_some() || f.optional;
                f.seen = now;
            }
        }
        changed
    }

    /// Run `load` if the file changed, returning what it loaded. `load` says
    /// `None` for a file it can't use, which leaves the running config alone;
    /// that version is not retried until the file changes again.
    pub fn poll<T>(&mut self, load: impl FnOnce() -> Option<T>) -> Option<T> {
        if self.changed() {
            load()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::tmpdir;
    use std::time::SystemTime;

    fn age(path: &std::path::Path, secs: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs)).unwrap();
    }

    #[test]
    fn an_untouched_file_is_not_a_change() {
        let dir = tmpdir("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut w = ConfigWatch::new(Some(path));
        assert!(!w.changed());
        assert!(!w.changed());
    }

    #[test]
    fn an_edit_is_seen_once_by_time_or_by_size() {
        let dir = tmpdir("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        age(&path, 100);
        let mut w = ConfigWatch::new(Some(path.clone()));
        // Same length, newer time.
        std::fs::write(&path, "a = 2\n").unwrap();
        assert!(w.changed(), "a newer mtime");
        assert!(!w.changed(), "reported once");
        // Same time, different length.
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::fs::write(&path, "a = 22\n").unwrap();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_modified(modified).unwrap();
        assert!(w.changed(), "a different size");
    }

    #[test]
    fn our_own_write_is_not_reloaded_twice() {
        let dir = tmpdir("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        age(&path, 100);
        let mut w = ConfigWatch::new(Some(path.clone()));
        std::fs::write(&path, "a = 2\n").unwrap();
        w.mark_seen();
        assert!(!w.changed());
    }

    #[test]
    fn a_missing_file_keeps_the_config_and_a_new_one_reloads() {
        let dir = tmpdir("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut w = ConfigWatch::new(Some(path.clone()));
        std::fs::remove_file(&path).unwrap();
        assert!(!w.changed(), "deleted: nothing to load");
        std::fs::write(&path, "a = 1\n").unwrap();
        assert!(w.changed(), "back again");
        let mut none = ConfigWatch::new(None);
        assert!(!none.changed());
    }

    #[test]
    fn an_overlay_edit_or_removal_is_a_change() {
        let dir = tmpdir("watch");
        let (path, local) = (dir.join("config.toml"), dir.join("config.local.toml"));
        std::fs::write(&path, "a = 1\n").unwrap();
        let mut w = ConfigWatch::new(Some(path)).with_overlay(Some(local.clone()));
        assert!(!w.changed(), "no overlay yet");
        std::fs::write(&local, "a = 2\n").unwrap();
        assert!(w.changed(), "created");
        assert!(!w.changed());
        std::fs::remove_file(&local).unwrap();
        assert!(w.changed(), "removed: the main file applies alone");
        assert!(!w.changed());
    }

    #[test]
    fn poll_loads_only_changed_files_and_does_not_retry_a_bad_one() {
        let dir = tmpdir("watch");
        let path = dir.join("config.toml");
        std::fs::write(&path, "a = 1\n").unwrap();
        age(&path, 100);
        let mut w = ConfigWatch::new(Some(path.clone()));
        assert_eq!(w.poll(|| Some(1)), None, "unchanged: no load");
        std::fs::write(&path, "not toml {{\n").unwrap();
        let mut tried = 0;
        assert_eq!(
            w.poll(|| {
                tried += 1;
                None::<i32>
            }),
            None,
            "an unusable file leaves the config"
        );
        assert_eq!(w.poll(|| Some(2)), None, "and is not retried");
        assert_eq!(tried, 1);
        std::fs::write(&path, "a = 3\n").unwrap();
        age(&path, 50);
        assert_eq!(w.poll(|| Some(3)), Some(3), "a fixed file loads");
    }
}
