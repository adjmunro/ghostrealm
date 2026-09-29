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

/// How the file browser opens a file when you click/Enter it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum OpenIn {
    /// Add a tab in the browser's own pane.
    Tab,
    /// Open beside the browser in a new horizontal split (when the pane is wide
    /// enough; otherwise falls back to a tab).
    #[default]
    Split,
}

/// `[browser]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Browser {
    /// Where opening a file puts it.
    pub open_in: OpenIn,
}

impl Default for Browser {
    fn default() -> Self {
        Browser { open_in: OpenIn::Split }
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
    /// Max gap (ms) between the two clicks of a mouse double-click (e.g. the file
    /// browser's double-click-to-enter-a-directory).
    pub double_click_ms: u32,
}

impl Default for Input {
    fn default() -> Self {
        Input {
            focus_follows_mouse: false,
            double_tap_window_ms: 300,
            double_click_ms: 200,
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

/// Line-number gutter mode for the text editor.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LineNumbers {
    /// No gutter.
    Off,
    /// Every line shows its absolute number.
    #[default]
    Absolute,
    /// Distance from the cursor's line; the cursor's own line shows its absolute
    /// number (not 0).
    Relative,
}

/// `[editor]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    /// Line-number gutter mode.
    pub line_numbers: LineNumbers,
    /// Highlight the line the cursor is on.
    pub cursor_line: bool,
    /// Default soft-wrap state for editor panes (toggle per-pane with the ribbon
    /// button). When off, long lines are clipped at the pane edge.
    pub soft_wrap: bool,
    /// Save a modified file when its editor loses focus (tab/workspace/app switch).
    pub autosave_on_unfocus: bool,
    /// Save a modified file after this many seconds of no edits (0 = off).
    pub autosave_after: u32,
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            line_numbers: LineNumbers::Absolute,
            cursor_line: true,
            soft_wrap: true,
            autosave_on_unfocus: true,
            autosave_after: 15,
        }
    }
}

/// `[workspace]`
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Workspace {
    /// Base directory new workspaces/terminals start in when the workspace has no
    /// pinned root. `None` (unset) means `$HOME`. `~` is expanded.
    pub default_directory: Option<String>,
}

/// The whole config.
#[derive(Clone, Debug, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub workspace: Workspace,
    pub sidebar: Sidebar,
    pub terminal: Terminal,
    /// Chrome colours for our own UI. Unset (`None`) means inherit from the user's
    /// Ghostty config (falling back to the built-in defaults) — see
    /// [`resolved_chrome`](Config::resolved_chrome).
    pub chrome: Option<Chrome>,
    pub tabs: Tabs,
    pub editor: Editor,
    pub input: Input,
    pub browser: Browser,
    pub inbox: Inbox,
    /// `[keybindings]`: command id -> chord, overlaying the built-in defaults.
    /// An empty table means "use built-ins". `palette.toggle` is a pseudo-id the
    /// app handles specially (open the command palette).
    pub keybindings: HashMap<String, String>,
}

impl Config {
    /// The resolved default directory for new workspaces (`~` expanded), or `None`
    /// (meaning `$HOME` / the shell's default).
    pub fn default_dir(&self) -> Option<PathBuf> {
        expand_tilde(self.workspace.default_directory.as_deref()?)
    }

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

    /// The chord bound to a command id (for showing shortcuts in the palette): a
    /// user override wins over the built-in default. Returns the canonical chord.
    /// The config's `[keybindings]` maps `command id -> chord`.
    pub fn binding_for(&self, id: &str) -> Option<String> {
        // The config's `[keybindings]` is authoritative when it has any *valid*
        // entry (a value that parses as a chord); otherwise — empty, or entirely
        // malformed (e.g. an old-format file) — the built-in defaults apply. This
        // keeps a bad edit from silently unbinding everything.
        if !self.has_valid_bindings() {
            return default_bindings()
                .into_iter()
                .find(|(_, v)| *v == id)
                .map(|(k, _)| k.to_string());
        }
        self.keybindings.get(id).and_then(|c| normalize_chord(c))
    }

    /// Resolve a chord (any modifier order/alias) to a command id. The config's
    /// `[keybindings]` (`command id -> chord`) is authoritative when it has any
    /// valid entry; else the built-in defaults apply.
    pub fn binding(&self, chord: &str) -> Option<String> {
        let target = normalize_chord(chord)?;
        if !self.has_valid_bindings() {
            return default_bindings()
                .get(target.as_str())
                .map(|s| s.to_string());
        }
        self.keybindings
            .iter()
            .find(|(_, c)| normalize_chord(c).as_deref() == Some(target.as_str()))
            .map(|(id, _)| id.clone())
    }

    /// Whether any keybinding entry has a value that parses as a chord. A file
    /// with no valid entries (empty, or all malformed) uses the built-in defaults.
    fn has_valid_bindings(&self) -> bool {
        self.keybindings.values().any(|c| normalize_chord(c).is_some())
    }
}

/// Built-in chord -> command id defaults (canonical chord form).
fn default_bindings() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        // The command palette opens on a double-tap of Shift (Run mode: double
        // Ctrl); it needs no chord default. Bind "palette.toggle" in config to add one.
        ("cmd+t", "surface.new"),
        ("cmd+w", "pane.close"),
        ("cmd+d", "split.leftright"),
        ("cmd+shift+d", "split.topbottom"),
        ("cmd+n", "tab.new"),
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
pub const DEFAULT_CONFIG_TOML: &str = r#"# ghostrealm config. Commit/sync this as a dotfile. Each option below lists its
# type, allowed values/range, and default. Edit + save (Cmd+S) to hot-reload.

[workspace]
# default_directory (string path | unset) — base directory new workspaces start
#   in (a workspace's own pinned root overrides it). `~` is expanded. Unset = $HOME.
# default_directory = "~/Developer"

[sidebar]
side = "left"    # "left" | "right"       (default "left")   — which edge the sidebar sits on
width = 190.0    # number, logical px, > 0 (default 190.0)   — sidebar width

[terminal]
font_size = 15.0    # number, points, > 0 (default 15.0)
line_height = 18.0  # number, points, > 0 (default 18.0)   — row height (>= font_size)

# [chrome] — colours for our own UI (not the terminal contents). Each is RGB
#   [r, g, b], components 0..=255. Unset (the default): inherit from your Ghostty
#   config (background/foreground/cursor), falling back to the values below.
#   Uncomment to override.
# [chrome]
# background = [20, 20, 24]   # behind panes / split dividers
# sidebar = [24, 24, 30]
# accent = [90, 140, 220]     # focused-pane border, palette selection

[tabs]
autohide_single_tab = true  # bool (default true) — hide a pane's tab strip when it has one terminal

[editor]
line_numbers = "absolute"  # "off" | "absolute" | "relative" (default absolute) — gutter line numbers; relative shows the absolute number on the cursor line
cursor_line = true         # bool (default true) — highlight the line the cursor is on
soft_wrap = true           # bool (default true) — wrap long lines (toggle per-pane with the ribbon button)
autosave_on_unfocus = true # bool (default true) — save a modified file when its editor loses focus
autosave_after = 15        # integer seconds, >= 0 (default 15) — save a modified file after this idle time (0 = off)

[input]
focus_follows_mouse = false  # bool (default false)          — hovering a pane focuses it
double_tap_window_ms = 300   # integer ms, >= 0 (default 300) — max gap between a double-tap's two taps
double_click_ms = 200        # integer ms, >= 0 (default 200) — max gap between a mouse double-click's two clicks

[browser]
open_in = "split"            # "split" | "tab" (default "split") — where opening a file
                             #   in the file browser puts it: a horizontal split beside
                             #   the browser (falls back to a tab when the pane is narrow),
                             #   or a tab in the browser's own pane

[inbox]
auto_read_after = 3     # integer seconds, >= 0 (default 3)  — focus time before unread -> read (0 = manual only)
auto_unread_before = 1  # integer seconds, >= 0 (default 1)  — grace after an auto-read to re-mark unread on unfocus

[keybindings]
# "command id" = "chord". Cmd chords only (other keys go to the terminal); any
# modifier order/alias works (cmd/super/meta, opt/alt). This section is the
# complete, authoritative set of bindings — every command is listed below, bound
# ones as active lines and unbound ones commented out. Edit a chord to rebind,
# delete or comment a line to unbind, or uncomment a line and set its chord to
# bind it. If the whole section is empty, the built-in defaults apply.
"#;

/// Expand a user-entered directory: trims whitespace, expands a leading `~` to
/// `$HOME`; `None` for an empty string. Does not check existence.
pub fn expand_tilde(s: &str) -> Option<PathBuf> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(rest) = s.strip_prefix('~') {
        std::env::var_os("HOME").map(|h| PathBuf::from(h).join(rest.trim_start_matches('/')))
    } else {
        Some(PathBuf::from(s))
    }
}

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
/// Command ids that carry a built-in default binding. Callers building the
/// keybindings list can assert every one is a listed command, so a default can
/// never point at an action missing from the generated block.
pub fn default_binding_ids() -> Vec<&'static str> {
    default_bindings().into_values().collect()
}

/// The default config file text with a generated, authoritative `[keybindings]`
/// block: every command listed (deterministically, sorted by id), bound ones as
/// active lines and unbound ones commented out. `commands` is `(id, title)` for
/// every keybindable command — pass the registry's, so the block can never drift
/// out of sync with the available actions. An empty `commands` yields the bare
/// template (empty section → built-in defaults apply).
pub fn default_config_with_keybindings(commands: &[(&str, &str)]) -> String {
    format!("{DEFAULT_CONFIG_TOML}{}", keybindings_lines(commands))
}

/// Just the generated `[keybindings]` body — one line per command, sorted by id,
/// bound ones active and unbound ones commented out.
fn keybindings_lines(commands: &[(&str, &str)]) -> String {
    let defaults = default_bindings();
    let chord_for = |id: &str| -> Option<&'static str> {
        defaults.iter().find(|&(_, &v)| v == id).map(|(&k, _)| k)
    };
    let mut cmds: Vec<(&str, &str)> = commands.to_vec();
    cmds.sort_by(|a, b| a.0.cmp(b.0));
    cmds.dedup_by(|a, b| a.0 == b.0);
    let mut out = String::new();
    for (id, title) in cmds {
        match chord_for(id) {
            Some(chord) => out.push_str(&format!("\"{id}\" = \"{chord}\"  # {title}\n")),
            None => out.push_str(&format!("# \"{id}\" = \"cmd+?\"  # {title} (unbound)\n")),
        }
    }
    out
}

/// Top-level `[section]` names that are active (uncommented) headers in `text`.
fn active_section_headers(text: &str) -> std::collections::HashSet<String> {
    text.lines()
        .filter_map(|l| {
            let t = l.trim();
            if t.starts_with('[') && !t.starts_with("[[") && t.ends_with(']') {
                Some(t[1..t.len() - 1].trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

/// Split [`DEFAULT_CONFIG_TOML`] into `(section_name, block_text)` per top-level
/// `[section]`, in order. The preamble before the first section is dropped.
fn template_sections() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for line in DEFAULT_CONFIG_TOML.lines() {
        let t = line.trim();
        let is_header =
            t.starts_with('[') && !t.starts_with("[[") && t.ends_with(']') && !t.starts_with('#');
        if is_header {
            if let Some(sec) = cur.take() {
                out.push(sec);
            }
            let name = t[1..t.len() - 1].trim().to_string();
            cur = Some((name, format!("{line}\n")));
        } else if let Some((_, block)) = cur.as_mut() {
            block.push_str(line);
            block.push('\n');
        }
    }
    if let Some(sec) = cur.take() {
        out.push(sec);
    }
    out
}

/// Whether the `[keybindings]` section of `text` has any active (uncommented)
/// binding line (a quoted chord key).
fn has_active_keybinding(text: &str) -> bool {
    let mut in_kb = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') && t.ends_with(']') && !t.starts_with("[[") {
            in_kb = &t[1..t.len() - 1] == "keybindings";
            continue;
        }
        if in_kb && t.starts_with('"') {
            return true;
        }
    }
    false
}

/// Bring existing config `text` up to date without disturbing any existing line:
/// append every documented top-level section the file is missing, and if the
/// `[keybindings]` section has no active bindings, add the generated block
/// (inserted under an existing header, else appended as a new section). Returns
/// the new text, or `None` if nothing needed adding.
pub fn backfill_text(text: &str, commands: &[(&str, &str)]) -> Option<String> {
    // Never rewrite a file that doesn't parse — a broken edit shouldn't be
    // "fixed" by us clobbering it.
    if toml::from_str::<Config>(text).is_err() {
        return None;
    }
    let present = active_section_headers(text);
    let sections = template_sections();
    let mut result = text.to_string();
    let mut changed = false;

    for (name, block) in &sections {
        if name == "keybindings" || present.contains(name) {
            continue;
        }
        if !result.ends_with('\n') {
            result.push('\n');
        }
        result.push('\n');
        result.push_str(block.trim_end());
        result.push('\n');
        changed = true;
    }

    if !has_active_keybinding(text) {
        let lines = keybindings_lines(commands);
        if present.contains("keybindings") {
            // Insert the generated bindings right after the existing header so they
            // fall under the [keybindings] table.
            let mut rebuilt = String::new();
            for line in result.lines() {
                rebuilt.push_str(line);
                rebuilt.push('\n');
                if line.trim() == "[keybindings]" {
                    rebuilt.push_str(&lines);
                }
            }
            result = rebuilt;
        } else {
            let header = sections
                .iter()
                .find(|(n, _)| n == "keybindings")
                .map(|(_, b)| b.clone())
                .unwrap_or_else(|| "[keybindings]\n".to_string());
            if !result.ends_with('\n') {
                result.push('\n');
            }
            result.push('\n');
            result.push_str(header.trim_end());
            result.push('\n');
            result.push_str(&lines);
        }
        changed = true;
    }

    changed.then_some(result)
}

/// Backfill the on-disk config file in place (see [`backfill_text`]). Returns
/// whether it changed anything.
pub fn backfill_config(commands: &[(&str, &str)]) -> bool {
    let Some(path) = config_path() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return false;
    };
    match backfill_text(&text, commands) {
        Some(updated) => std::fs::write(&path, updated).is_ok(),
        None => false,
    }
}

pub fn load_or_create() -> Config {
    load_or_create_with(&[])
}

/// Like [`load_or_create`], but seeds a freshly created config with a generated
/// `[keybindings]` block covering every command in `commands`.
pub fn load_or_create_with(commands: &[(&str, &str)]) -> Config {
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
            let _ = std::fs::write(&path, default_config_with_keybindings(commands));
            Config::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backfill_adds_missing_sections_and_keybindings() {
        // A sparse, out-of-date config: a customised [terminal] and an empty
        // [keybindings]. Nothing else.
        let old = "[terminal]\nfont_size = 20.0\n\n[keybindings]\n# (empty)\n";
        let cmds = [
            ("surface.new", "New Terminal Tab"),
            ("tab.new", "New Workspace"),
            ("editor.scratch", "New Editor"),
        ];
        let updated = backfill_text(old, &cmds).expect("backfill should add sections");
        let cfg: Config = toml::from_str(&updated).expect("backfilled config parses");

        // The user's value is preserved untouched.
        assert!((cfg.terminal.font_size - 20.0).abs() < 1e-6);
        // Missing sections were appended.
        for sec in ["[sidebar]", "[editor]", "[input]", "[inbox]", "[tabs]"] {
            assert!(updated.contains(sec), "backfill adds {sec}");
        }
        // The authoritative keybindings block was seeded (so bindings resolve).
        assert!(!cfg.keybindings.is_empty());
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("surface.new"));
        assert_eq!(cfg.binding("cmd+n").as_deref(), Some("tab.new"));
        // An unbound command appears as a comment for reference.
        assert!(updated.contains("editor.scratch"));
        // Idempotent: a second pass changes nothing.
        assert!(
            backfill_text(&updated, &cmds).is_none(),
            "backfill is idempotent"
        );
    }

    #[test]
    fn backfill_leaves_a_current_config_untouched() {
        let full = default_config_with_keybindings(&[("surface.new", "New Terminal Tab")]);
        assert!(
            backfill_text(&full, &[("surface.new", "New Terminal Tab")]).is_none(),
            "a freshly generated config needs no backfill"
        );
    }

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
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("surface.new"));
        assert_eq!(cfg.binding("cmd+n").as_deref(), Some("tab.new"));
        // cmd+k is no longer a default (the palette opens on double-Shift).
        assert_eq!(cfg.binding("cmd+k"), None);
        assert_eq!(cfg.binding("cmd+j"), None);
        // Authoritative once present: `command id = chord`. Any modifier
        // order/alias resolves; ids not listed are unbound.
        cfg.keybindings
            .insert("tab.close".to_string(), "Super+T".to_string());
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("tab.close"));
        assert_eq!(cfg.binding_for("tab.close").as_deref(), Some("cmd+t"));
        assert_eq!(cfg.binding("cmd+n"), None); // not in the authoritative set
    }
}


