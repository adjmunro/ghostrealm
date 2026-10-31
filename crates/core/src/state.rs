//! App-generated state (what the app learns as it runs, not what the user
//! sets): one JSON file per feature in the platform state dir, never synced.
//! A file per feature means features never contend for one file, and a
//! corrupt file costs only its own feature, which starts from its defaults.

use std::ffi::OsString;
use std::path::PathBuf;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A directory of per-feature state files.
#[derive(Clone, Debug)]
pub struct StateStore {
    dir: PathBuf,
}

impl StateStore {
    /// The store in `dir`.
    pub fn new(dir: PathBuf) -> Self {
        StateStore { dir }
    }

    /// The store in the platform state dir ([`state_dir`]).
    pub fn platform() -> Option<Self> {
        state_dir().map(StateStore::new)
    }

    /// `<dir>/<feature>.json`. `None` for a feature name that isn't
    /// `[a-z0-9_-]+`, so a name never reaches outside the dir.
    fn file(&self, feature: &str) -> Option<PathBuf> {
        let ok = !feature.is_empty()
            && feature.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        ok.then(|| self.dir.join(format!("{feature}.json")))
    }

    /// `feature`'s saved state. `None` when there is none, or it can't be
    /// read or parsed (the feature starts from its defaults).
    pub fn load<T: DeserializeOwned>(&self, feature: &str) -> Option<T> {
        let path = self.file(feature)?;
        let text = std::fs::read_to_string(&path).ok()?;
        match serde_json::from_str(&text) {
            Ok(value) => Some(value),
            Err(e) => {
                eprintln!("ghostrealm: ignoring {}: {e}", path.display());
                None
            }
        }
    }

    /// Save `feature`'s state, replacing what was saved.
    pub fn save<T: Serialize>(&self, feature: &str, value: &T) -> Result<(), String> {
        let path = self.file(feature).ok_or_else(|| format!("bad state feature name `{feature}`"))?;
        std::fs::create_dir_all(&self.dir).map_err(|e| format!("can't create {}: {e}", self.dir.display()))?;
        let json = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
        let permissions = std::fs::metadata(&path).ok().map(|m| m.permissions());
        crate::config::write::replace(&path, &json, permissions)
    }
}

/// The state dir: `$XDG_STATE_HOME/ghostrealm/state`, else on macOS
/// `~/Library/Application Support/ghostrealm/state`, else
/// `~/.local/state/ghostrealm/state`.
pub fn state_dir() -> Option<PathBuf> {
    state_dir_from(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME"), cfg!(target_os = "macos"))
}

fn state_dir_from(xdg: Option<OsString>, home: Option<OsString>, macos: bool) -> Option<PathBuf> {
    let base = match xdg.filter(|p| !p.is_empty()) {
        Some(xdg) => PathBuf::from(xdg),
        None => {
            let home = PathBuf::from(home.filter(|h| !h.is_empty())?);
            match macos {
                true => home.join("Library").join("Application Support"),
                false => home.join(".local").join("state"),
            }
        }
    };
    Some(base.join("ghostrealm").join("state"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TmpDir;
    use std::collections::BTreeMap;

    #[test]
    fn a_feature_round_trips() {
        let dir = TmpDir::new("state");
        let store = StateStore::new(dir.join("state"));
        assert_eq!(store.load::<Vec<String>>("mru"), None, "nothing saved yet");
        let mru = vec!["pane.close".to_string(), "surface.new".to_string()];
        store.save("mru", &mru).unwrap();
        assert_eq!(store.load::<Vec<String>>("mru"), Some(mru));
        assert!(dir.join("state").join("mru.json").is_file());
    }

    #[test]
    fn a_corrupt_file_costs_only_its_feature() {
        let dir = TmpDir::new("state");
        let store = StateStore::new(dir.to_path_buf());
        let sizes = BTreeMap::from([("w".to_string(), 1200u32), ("h".to_string(), 800)]);
        store.save("window", &sizes).unwrap();
        store.save("mru", &vec!["a"]).unwrap();
        std::fs::write(dir.join("mru.json"), "[\"a\", tru").unwrap();
        assert_eq!(store.load::<Vec<String>>("mru"), None);
        assert_eq!(store.load::<BTreeMap<String, u32>>("window"), Some(sizes));
        // The next save replaces the corrupt file.
        store.save("mru", &vec!["b"]).unwrap();
        assert_eq!(store.load::<Vec<String>>("mru"), Some(vec!["b".to_string()]));
    }

    #[test]
    fn a_feature_name_stays_inside_the_dir() {
        let dir = TmpDir::new("state");
        let store = StateStore::new(dir.join("state"));
        for bad in ["", "../x", "a/b", "A", "x.json"] {
            assert!(store.save(bad, &1).is_err(), "{bad:?}");
            assert_eq!(store.load::<i32>(bad), None);
        }
        assert!(!dir.join("x.json").exists());
    }

    #[test]
    fn the_state_dir_follows_xdg_then_the_platform() {
        let home = || Some(OsString::from("/h"));
        let at = |p: &str| Some(PathBuf::from(p));
        assert_eq!(state_dir_from(Some("/x".into()), home(), true), at("/x/ghostrealm/state"));
        assert_eq!(state_dir_from(Some("".into()), home(), false), at("/h/.local/state/ghostrealm/state"));
        assert_eq!(state_dir_from(None, home(), true), at("/h/Library/Application Support/ghostrealm/state"));
        assert_eq!(state_dir_from(None, None, false), None);
    }
}
