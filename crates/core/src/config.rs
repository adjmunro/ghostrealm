//! The TOML config: a single shareable dotfile at an XDG path. Missing fields
//! fall back to defaults (`#[serde(default)]`), so a partial file is valid and
//! new keys don't break old files. On first run the documented default is
//! written out.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Which side the vertical tab sidebar sits on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Left,
    Right,
}

/// `[sidebar]`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sidebar {
    pub side: Side,
    /// Width in logical pixels.
    pub width: f32,
}

impl Default for Sidebar {
    fn default() -> Self {
        Sidebar {
            side: Side::Left,
            width: 190.0,
        }
    }
}

/// `[terminal]`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub font_size: f32,
    pub line_height: f32,
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            font_size: 15.0,
            line_height: 18.0,
        }
    }
}

/// `[chrome]` — colours for our own UI (not the terminal contents). RGB arrays.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Chrome {
    /// Divider/background behind panes.
    pub background: [u8; 3],
    /// Sidebar background.
    pub sidebar: [u8; 3],
    /// Focused-pane border and selection accent.
    pub accent: [u8; 3],
}

impl Default for Chrome {
    fn default() -> Self {
        Chrome {
            background: [20, 20, 24],
            sidebar: [24, 24, 30],
            accent: [90, 140, 220],
        }
    }
}

/// `[inbox]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Inbox {
    /// Seconds of continuous focus before an unread tab becomes read (0 = manual).
    pub auto_read_after: u32,
    /// Grace seconds: unfocusing within this window after a read re-marks unread.
    pub auto_unread_before: u32,
}

impl Default for Inbox {
    fn default() -> Self {
        Inbox {
            auto_read_after: 60,
            auto_unread_before: 10,
        }
    }
}

/// `[tabs]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Tabs {
    /// Hide a pane's horizontal tab strip when it holds only one terminal.
    pub autohide_single_tab: bool,
}

impl Default for Tabs {
    fn default() -> Self {
        Tabs {
            autohide_single_tab: true,
        }
    }
}

/// The whole config.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub sidebar: Sidebar,
    pub terminal: Terminal,
    pub chrome: Chrome,
    pub tabs: Tabs,
    pub inbox: Inbox,
}

/// The default config written on first run. Kept in sync with [`Config::default`]
/// by a test, so the comments here document the real defaults.
pub const DEFAULT_CONFIG_TOML: &str = r#"# ghostrealm config. Commit/sync this as a dotfile.

[sidebar]
side = "left"    # "left" or "right"
width = 190.0    # logical pixels

[terminal]
font_size = 15.0
line_height = 18.0

[chrome]
# RGB [r, g, b]. Chrome colours for our own UI, not the terminal contents.
background = [20, 20, 24]   # behind panes / split dividers
sidebar = [24, 24, 30]
accent = [90, 140, 220]     # focused-pane border, palette selection

[tabs]
autohide_single_tab = true  # hide a pane's tab strip when it has one terminal

[inbox]
auto_read_after = 60    # seconds of focus before unread -> read (0 = manual only)
auto_unread_before = 10 # grace seconds after read to re-mark unread on unfocus
"#;

/// The config file path: `$XDG_CONFIG_HOME/ghostrealm/config.toml`, falling back
/// to `~/.config/ghostrealm/config.toml`.
pub fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("ghostrealm").join("config.toml"))
}

/// Load the config, creating a documented default file if none exists. A parse
/// error is reported and the built-in defaults are used (never fatal).
pub fn load_or_create() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => match toml::from_str(&text) {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!(
                    "ghostrealm: {} is invalid ({e}); using defaults",
                    path.display()
                );
                Config::default()
            }
        },
        Err(_) => {
            if let Some(dir) = path.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            let _ = std::fs::write(&path, DEFAULT_CONFIG_TOML);
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_toml_matches_default_config() {
        let parsed: Config = toml::from_str(DEFAULT_CONFIG_TOML).expect("default toml parses");
        assert_eq!(
            parsed,
            Config::default(),
            "DEFAULT_CONFIG_TOML drifted from Config::default(); update the template/comments."
        );
    }

    #[test]
    fn partial_config_fills_defaults() {
        let cfg: Config = toml::from_str("[terminal]\nfont_size = 20.0\n").unwrap();
        assert_eq!(cfg.terminal.font_size, 20.0);
        assert_eq!(cfg.terminal.line_height, Terminal::default().line_height);
        assert_eq!(cfg.sidebar, Sidebar::default());
    }

    #[test]
    fn unknown_keys_are_ignored() {
        // Forward-compatible: an old build reading a newer file must not fail.
        let cfg: Config = toml::from_str("[terminal]\nfuture_key = 1\n").unwrap();
        assert_eq!(cfg.terminal, Terminal::default());
    }

    #[test]
    fn round_trips() {
        let cfg = Config::default();
        let text = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }
}
