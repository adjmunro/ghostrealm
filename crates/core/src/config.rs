//! The TOML config: a single shareable dotfile at an XDG path. Missing fields
//! fall back to defaults (`#[serde(default)]`), so a partial file is valid and
//! new keys don't break old files. On first run the documented default is
//! written out.

use std::collections::HashMap;
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
            auto_read_after: 3,
            auto_unread_before: 1,
        }
    }
}

/// `[input]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Input {
    /// Hovering a pane focuses it (routes keyboard there). Off by default.
    pub focus_follows_mouse: bool,
    /// Max gap (ms) between the two taps of a double-tap trigger (palette / run).
    pub double_tap_window_ms: u32,
}

impl Default for Input {
    fn default() -> Self {
        Input {
            focus_follows_mouse: false,
            double_tap_window_ms: 300,
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
    /// Chrome colours for our own UI. Unset (`None`) means inherit from the user's
    /// Ghostty config (falling back to the built-in defaults) — see
    /// [`resolved_chrome`](Config::resolved_chrome).
    pub chrome: Option<Chrome>,
    pub tabs: Tabs,
    pub input: Input,
    pub inbox: Inbox,
    /// `[keybindings]`: chord -> command id, overlaying the built-in defaults.
    /// An empty table means "use built-ins". `palette.toggle` is a pseudo-id the
    /// app handles specially (open the command palette).
    pub keybindings: HashMap<String, String>,
}

impl Config {
    /// The chrome colours to use: an explicit `[chrome]` if set, else colours
    /// inherited from the user's Ghostty config, else the built-in defaults.
    /// Does file I/O (reads the Ghostty config) when `[chrome]` is unset, so call
    /// once and cache.
    pub fn resolved_chrome(&self) -> Chrome {
        self.chrome
            .clone()
            .or_else(crate::ghostty::inherited_chrome)
            .unwrap_or_default()
    }

    /// Resolve a chord (any modifier order/alias) to a command id: a user
    /// override wins, else the built-in default, else `None`.
    pub fn binding(&self, chord: &str) -> Option<String> {
        let target = normalize_chord(chord)?;
        for (k, v) in &self.keybindings {
            if normalize_chord(k).as_deref() == Some(target.as_str()) {
                return Some(v.clone());
            }
        }
        default_bindings()
            .get(target.as_str())
            .map(|s| s.to_string())
    }
}

/// Built-in chord -> command id defaults (canonical chord form).
fn default_bindings() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        ("cmd+k", "palette.toggle"),
        ("cmd+t", "tab.new"),
        ("cmd+w", "pane.close"),
        ("cmd+d", "split.leftright"),
        ("cmd+shift+d", "split.topbottom"),
        ("cmd+n", "surface.new"),
        ("cmd+]", "pane.focus_next"),
        ("cmd+[", "pane.focus_prev"),
    ])
}

/// Canonicalise a chord string: lowercase, modifier aliases folded, fixed
/// modifier order (`ctrl+alt+shift+cmd+<key>`). Returns `None` if it has no key
/// or more than one non-modifier token.
pub fn normalize_chord(chord: &str) -> Option<String> {
    let (mut ctrl, mut alt, mut shift, mut cmd) = (false, false, false, false);
    let mut key: Option<String> = None;
    for tok in chord.split('+') {
        match tok.trim().to_ascii_lowercase().as_str() {
            "" => {}
            "ctrl" | "control" => ctrl = true,
            "alt" | "opt" | "option" => alt = true,
            "shift" => shift = true,
            "cmd" | "super" | "win" | "meta" | "command" => cmd = true,
            other => {
                if key.is_some() {
                    return None;
                }
                key = Some(other.to_string());
            }
        }
    }
    let key = key?;
    let mut s = String::new();
    if ctrl {
        s.push_str("ctrl+");
    }
    if alt {
        s.push_str("alt+");
    }
    if shift {
        s.push_str("shift+");
    }
    if cmd {
        s.push_str("cmd+");
    }
    s.push_str(&key);
    Some(s)
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

# [chrome] — colours for our own UI, not the terminal contents. RGB [r, g, b].
# Unset (the default): inherit from your Ghostty config (background/foreground/
# cursor), falling back to the values below. Uncomment to override.
# [chrome]
# background = [20, 20, 24]   # behind panes / split dividers
# sidebar = [24, 24, 30]
# accent = [90, 140, 220]     # focused-pane border, palette selection

[tabs]
autohide_single_tab = true  # hide a pane's tab strip when it has one terminal

[input]
focus_follows_mouse = false  # hovering a pane focuses it (routes keyboard there)
double_tap_window_ms = 300    # max gap between the two taps of a double-tap trigger

[inbox]
auto_read_after = 3     # seconds of focus before unread -> read (0 = manual only)
auto_unread_before = 1  # grace seconds after an auto-read to re-mark unread on unfocus

[keybindings]
# Override chord -> command id (Cmd chords only; others go to the terminal).
# Unset entries use the built-in defaults:
#   "cmd+k" = "palette.toggle"
#   "cmd+t" = "tab.new"
#   "cmd+w" = "pane.close"
#   "cmd+d" = "split.leftright"
#   "cmd+shift+d" = "split.topbottom"
#   "cmd+n" = "surface.new"
#   "cmd+]" = "pane.focus_next"
#   "cmd+[" = "pane.focus_prev"
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

    #[test]
    fn normalize_chord_canonicalises_order_and_aliases() {
        assert_eq!(
            normalize_chord("Shift+Super+D").as_deref(),
            Some("shift+cmd+d")
        );
        assert_eq!(normalize_chord("command+t").as_deref(), Some("cmd+t"));
        assert_eq!(normalize_chord("ctrl").as_deref(), None); // no key
    }

    #[test]
    fn binding_uses_defaults_then_overrides() {
        let mut cfg = Config::default();
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("tab.new"));
        assert_eq!(cfg.binding("cmd+k").as_deref(), Some("palette.toggle"));
        assert_eq!(cfg.binding("cmd+j"), None);
        // A user override wins, in any modifier order/alias.
        cfg.keybindings
            .insert("Super+T".to_string(), "tab.close".to_string());
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("tab.close"));
    }
}
