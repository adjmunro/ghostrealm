//! Write one setting into `config.toml` on an explicit user action, keeping
//! the file's comments, order and formatting (`toml_edit`).
//!
//! The file is the user's, so any doubt about what it holds leaves it as it
//! is: a file that can't be read, is empty, isn't valid TOML, is read-only, or
//! holds something other than a table where the section goes is not written.
//! The new text goes to a temp file beside the real file and is renamed over
//! it, so a failed write never truncates it; a symlinked config is written
//! through the link (a dotfiles checkout keeps its file).

use std::io::Write;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, Table};

/// Set `[section] key = value` in the config file at `path`.
pub fn set(path: &Path, section: &str, key: &str, value: &toml::Value) -> Result<(), String> {
    let real = path.canonicalize().map_err(|e| format!("can't read {}: {e}", path.display()))?;
    let name = real.display();
    let meta = std::fs::metadata(&real).map_err(|e| format!("can't read {name}: {e}"))?;
    if meta.permissions().readonly() {
        return Err(format!("{name} is read-only"));
    }
    let text = std::fs::read_to_string(&real).map_err(|e| format!("can't read {name}: {e}"))?;
    if text.is_empty() {
        return Err(format!("{name} is empty"));
    }
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("{name} isn't valid TOML: {e}"))?;
    let value: toml_edit::Value = value.to_string().parse().map_err(|e| format!("bad value for {key}: {e}"))?;
    let entry = doc.entry(section).or_insert_with(|| Item::Table(Table::new()));
    let Some(table) = entry.as_table_like_mut() else {
        return Err(format!("`{section}` in {name} isn't a table"));
    };
    match table.get_mut(key) {
        // Keep the old value's surrounding whitespace and trailing comment.
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = value;
            *old.decor_mut() = decor;
        }
        Some(_) => return Err(format!("`{section}.{key}` in {name} isn't a value")),
        None => {
            table.insert(key, Item::Value(value));
        }
    }
    replace(&real, doc.to_string().as_bytes(), Some(meta.permissions()))
}

/// Write `bytes` to a temp file beside `path` and rename it over `path`,
/// giving it `permissions` when set.
pub(crate) fn replace(path: &Path, bytes: &[u8], permissions: Option<std::fs::Permissions>) -> Result<(), String> {
    let tmp = temp_beside(path);
    let written = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        if let Some(p) = permissions {
            std::fs::set_permissions(&tmp, p)?;
        }
        std::fs::rename(&tmp, path)
    })();
    written.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("can't write {}: {e}", path.display())
    })
}

/// `.<name>.ghostrealm-tmp` in `path`'s directory.
fn temp_beside(path: &Path) -> PathBuf {
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    path.with_file_name(format!(".{name}.ghostrealm-tmp"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_dir::TmpDir;

    const TEXT: &str = "# my settings\n\n[terminal]\nfont_size = 13 # small\n\n# files\n[file_browser]\nsidebar = \"hidden\"\npreview_autoplay = true  # on\n\n[keybindings]\n\"pane.close\" = \"cmd+w\"\n";

    fn file(dir: &TmpDir, text: &str) -> PathBuf {
        let path = dir.join("config.toml");
        std::fs::write(&path, text).unwrap();
        path
    }

    fn off() -> toml::Value {
        toml::Value::Boolean(false)
    }

    #[test]
    fn a_write_keeps_comments_order_and_the_values_trailing_comment() {
        let dir = TmpDir::new("config-write");
        let path = file(&dir, TEXT);
        set(&path, "file_browser", "preview_autoplay", &off()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TEXT.replace("preview_autoplay = true", "preview_autoplay = false"));
    }

    #[test]
    fn a_missing_key_and_section_are_added_at_the_end() {
        let dir = TmpDir::new("config-write");
        let path = file(&dir, "# mine\n[terminal]\nfont_size = 13\n");
        set(&path, "terminal", "line_height", &toml::Value::Float(20.0)).unwrap();
        set(&path, "file_browser", "preview_autoplay", &off()).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# mine\n[terminal]\nfont_size = 13\nline_height = 20.0\n\n[file_browser]\npreview_autoplay = false\n"
        );
        assert!(!dir.join(".config.toml.ghostrealm-tmp").exists(), "no temp file left");
    }

    /// `set` fails on `path` and leaves the file's bytes as they were.
    fn refused(path: &Path, why: &str) {
        let before = std::fs::read(path).ok();
        let err = set(path, "file_browser", "preview_autoplay", &off()).unwrap_err();
        assert!(err.contains(why), "{err}");
        assert_eq!(std::fs::read(path).ok(), before, "kept");
    }

    #[test]
    fn a_missing_file_is_not_created() {
        let dir = TmpDir::new("config-write");
        refused(&dir.join("config.toml"), "can't read");
        assert!(!dir.join("config.toml").exists());
    }

    #[test]
    fn an_unreadable_path_is_kept() {
        let dir = TmpDir::new("config-write");
        std::fs::create_dir(dir.join("config.toml")).unwrap();
        refused(&dir.join("config.toml"), "can't read");
        assert!(dir.join("config.toml").is_dir());
    }

    #[test]
    fn an_empty_file_is_kept() {
        let dir = TmpDir::new("config-write");
        refused(&file(&dir, ""), "empty");
    }

    #[test]
    fn a_file_that_isnt_toml_is_kept() {
        let dir = TmpDir::new("config-write");
        refused(&file(&dir, "[file_browser]\npreview_autoplay = tr"), "valid TOML");
    }

    #[test]
    fn a_section_that_isnt_a_table_is_kept() {
        let dir = TmpDir::new("config-write");
        refused(&file(&dir, "file_browser = 1\n"), "isn't a table");
        refused(&file(&dir, "[file_browser.preview_autoplay]\nx = 1\n"), "isn't a value");
    }

    #[test]
    fn a_read_only_file_is_kept() {
        let dir = TmpDir::new("config-write");
        let path = file(&dir, TEXT);
        let mut p = std::fs::metadata(&path).unwrap().permissions();
        p.set_readonly(true);
        std::fs::set_permissions(&path, p.clone()).unwrap();
        refused(&path, "read-only");
        #[allow(clippy::permissions_set_readonly_false)]
        p.set_readonly(false);
        std::fs::set_permissions(&path, p).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_file_is_written_through_its_link() {
        let dir = TmpDir::new("config-write");
        std::fs::create_dir(dir.join("dotfiles")).unwrap();
        let real = dir.join("dotfiles").join("config.toml");
        std::fs::write(&real, TEXT).unwrap();
        let link = dir.join("config.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        set(&link, "file_browser", "preview_autoplay", &off()).unwrap();
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "still a link");
        assert!(std::fs::read_to_string(&real).unwrap().contains("preview_autoplay = false  # on"));
    }
}
