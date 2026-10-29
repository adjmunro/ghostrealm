//! The TOML config: a single shareable dotfile at an XDG path. It lists only
//! what the user sets; missing fields fall back to defaults
//! (`#[serde(default)]`). Every option is described in the [`catalogue`], which
//! the [`check`]er (shared by the loader and the config language server) uses
//! to flag and drop bad entries without rejecting the whole file.

pub mod catalogue;
pub mod check;

use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub use catalogue::{core_sections, Catalogue, Entries, KeyDoc, Kind, OptionSpec, SectionSpec};
pub use check::{check, hover_at, Checked, Finding, Severity};

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

/// When a terminal closes by itself because its shell exited.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum CloseOnExit {
    /// Only after an exit with code 0; a failure or a signal stays on screen.
    #[default]
    Clean,
    /// After any exit.
    Always,
    /// Never: the terminal stays until it is closed.
    Never,
}

impl CloseOnExit {
    /// Whether to close a terminal whose shell exited cleanly (code 0) or not.
    pub fn closes(self, clean_exit: bool) -> bool {
        match self {
            CloseOnExit::Clean => clean_exit,
            CloseOnExit::Always => true,
            CloseOnExit::Never => false,
        }
    }
}

/// `[terminal]`
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Terminal {
    pub font_size: f32,
    pub line_height: f32,
    /// When the terminal closes after its shell exits.
    pub close_on_exit: CloseOnExit,
}

impl Default for Terminal {
    fn default() -> Self {
        Terminal {
            font_size: 15.0,
            line_height: 18.0,
            close_on_exit: CloseOnExit::Clean,
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

/// Whether a new Files surface shows its file sidebar beside a document.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SidebarPin {
    #[default]
    Pinned,
    Hidden,
}

/// `[file_browser]`
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FileBrowser {
    /// Whether a new Files surface shows the sidebar beside its document,
    /// until the sidebar is toggled this session.
    pub sidebar: SidebarPin,
    /// Start playing audio once its document has shown it for a moment.
    pub preview_autoplay: bool,
}

impl Default for FileBrowser {
    fn default() -> Self {
        FileBrowser {
            sidebar: SidebarPin::Pinned,
            preview_autoplay: true,
        }
    }
}

/// `[web]`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Web {
    /// Page a new web view opens.
    pub home: String,
    /// Search URL for URL-bar input that isn't an address; `{}` is the query.
    pub search: String,
    /// The web host executable. `None` = beside the `ghostrealm` binary.
    pub host: Option<String>,
}

impl Default for Web {
    fn default() -> Self {
        Web {
            home: "https://duckduckgo.com".into(),
            search: "https://duckduckgo.com/?q={}".into(),
            host: None,
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
    /// Max gap (ms) between the two clicks of a mouse double-click (e.g. the file
    /// file browser's double-click-to-enter-a-directory).
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

/// A document option that can also show as an icon in the document header
/// (`[editor] header_icons`). Every option is always in the header's `⋯`
/// menu; a document shows the icons of the listed options it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HeaderIcon {
    /// The statistic (word count, ...; a click cycles it).
    Statistic,
    /// The autosave toggle.
    Autosave,
    /// The soft-wrap toggle.
    SoftWrap,
    /// The view-mode button, when the document has more than one mode.
    ViewMode,
    /// The autoplay toggle of an audio document.
    Autoplay,
}

/// `[editor]`
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    /// Line-number gutter mode.
    pub line_numbers: LineNumbers,
    /// Highlight the line the cursor is on.
    pub cursor_line: bool,
    /// Default soft-wrap state for editor panes (toggle per document with the header's
    /// button). When off, long lines are clipped at the pane edge.
    pub soft_wrap: bool,
    /// Save a modified file when its editor loses focus (tab/workspace/app switch).
    pub autosave_on_unfocus: bool,
    /// Save a modified file after this many seconds of no edits (0 = off).
    pub autosave_after: u32,
    /// The font family of a Preview's body text. `None` (unset) means the
    /// platform's serif.
    pub preview_font: Option<String>,
    /// The options a document header also shows as icons, left to right.
    pub header_icons: Vec<HeaderIcon>,
}

impl Default for Editor {
    fn default() -> Self {
        Editor {
            line_numbers: LineNumbers::Absolute,
            cursor_line: true,
            soft_wrap: true,
            autosave_on_unfocus: true,
            autosave_after: 15,
            preview_font: None,
            header_icons: vec![HeaderIcon::Statistic, HeaderIcon::Autosave, HeaderIcon::SoftWrap, HeaderIcon::ViewMode],
        }
    }
}

/// `[headings]`: how a document's headings draw in its Integrated and Preview
/// views. Per level (from `Highlighter::heading_level`, 1–6): the font size
/// and the space above, both in ems of the view's body text.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Headings {
    pub h1_scale: f32,
    pub h2_scale: f32,
    pub h3_scale: f32,
    pub h4_scale: f32,
    pub h5_scale: f32,
    pub h6_scale: f32,
    pub h1_space: f32,
    pub h2_space: f32,
    pub h3_space: f32,
    pub h4_space: f32,
    pub h5_space: f32,
    pub h6_space: f32,
}

impl Default for Headings {
    fn default() -> Self {
        Headings {
            h1_scale: 2.0,
            h2_scale: 1.6,
            h3_scale: 1.35,
            h4_scale: 1.2,
            h5_scale: 1.1,
            h6_scale: 1.0,
            h1_space: 1.6,
            h2_space: 1.4,
            h3_space: 1.2,
            h4_space: 1.1,
            h5_space: 1.0,
            h6_space: 1.0,
        }
    }
}

impl Headings {
    /// How much larger than body text a heading of `level` draws; 1 for no
    /// heading or a level past 6.
    pub fn scale(&self, level: Option<u8>) -> f32 {
        match level {
            Some(1) => self.h1_scale,
            Some(2) => self.h2_scale,
            Some(3) => self.h3_scale,
            Some(4) => self.h4_scale,
            Some(5) => self.h5_scale,
            Some(6) => self.h6_scale,
            _ => 1.0,
        }
    }

    /// The space above a heading of `level`, in ems of body text; 0 for no
    /// heading or a level past 6.
    pub fn space(&self, level: Option<u8>) -> f32 {
        match level {
            Some(1) => self.h1_space,
            Some(2) => self.h2_space,
            Some(3) => self.h3_space,
            Some(4) => self.h4_space,
            Some(5) => self.h5_space,
            Some(6) => self.h6_space,
            _ => 0.0,
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
    pub headings: Headings,
    pub input: Input,
    pub file_browser: FileBrowser,
    pub web: Web,
    pub inbox: Inbox,
    /// `[keybindings]`: command id -> chord. Only listed commands are bound: a
    /// file without the section binds nothing (the field default is empty, not
    /// [`Config::default`]'s). `palette.toggle` is a pseudo-id the app handles
    /// specially (open the command palette).
    #[serde(default)]
    pub keybindings: HashMap<String, String>,
}

/// What a new config file holds: every option at its default, plus the default
/// keybindings ([`default_file`] parses to exactly this).
impl Default for Config {
    fn default() -> Self {
        Config {
            workspace: Workspace::default(),
            sidebar: Sidebar::default(),
            terminal: Terminal::default(),
            chrome: None,
            tabs: Tabs::default(),
            editor: Editor::default(),
            headings: Headings::default(),
            input: Input::default(),
            file_browser: FileBrowser::default(),
            web: Web::default(),
            inbox: Inbox::default(),
            keybindings: default_bindings()
                .into_iter()
                .map(|(chord, id)| (id.to_string(), chord.to_string()))
                .collect(),
        }
    }
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

    /// The canonical chord bound to a command id (for showing shortcuts in the
    /// palette), if any.
    pub fn binding_for(&self, id: &str) -> Option<String> {
        self.keybindings.get(id).and_then(|c| normalize_chord(c))
    }

    /// The command id bound to a chord (any modifier order/alias), if any.
    pub fn binding(&self, chord: &str) -> Option<String> {
        let target = normalize_chord(chord)?;
        self.keybindings
            .iter()
            .find(|(_, c)| normalize_chord(c).as_deref() == Some(target.as_str()))
            .map(|(id, _)| id.clone())
    }
}

/// The chord -> command id bindings a new config file starts with (canonical
/// chord form).
fn default_bindings() -> HashMap<&'static str, &'static str> {
    HashMap::from([
        // The command palette opens on a double-tap of Shift (Run mode: double
        // Ctrl); it needs no chord default. Bind "palette.toggle" in config to add one.
        ("cmd+t", "surface.new"),
        ("cmd+w", "pane.close"),
        ("cmd+d", "split.leftright"),
        ("cmd+shift+d", "split.topbottom"),
        ("cmd+n", "workspace.new"),
        ("cmd+]", "pane.focus_next"),
        ("cmd+[", "pane.focus_prev"),
        ("cmd+l", "web.focus_url"),
        ("cmd+/", "editor.toggle_comment"),
        ("cmd+shift+c", "file.copy_path"),
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

/// Command ids that carry a default binding (written into a new config file).
pub fn default_binding_ids() -> Vec<&'static str> {
    default_bindings().into_values().collect()
}

/// The text of a new config file: the default keybindings and nothing else.
/// Every other option is documented on hover in the editor and takes its
/// default until set.
pub fn default_file() -> String {
    let mut bindings: Vec<(&str, &str)> = default_bindings().into_iter().map(|(c, id)| (id, c)).collect();
    bindings.sort();
    let mut s = String::from(
        "# ghostrealm config. Only what you set is listed; every other option uses\n\
         # its default. Hover a section or option for its docs.\n\n[keybindings]\n",
    );
    for (id, chord) in bindings {
        s.push_str(&format!("\"{id}\" = \"{chord}\"\n"));
    }
    s
}

/// Parse config `text` against `cat`. Flagged entries are dropped (so their
/// defaults apply) or clamped. `None` when the text isn't valid TOML or holds
/// a number out of TOML's range.
pub fn parse(text: &str, cat: &Catalogue) -> (Option<Config>, Vec<Finding>) {
    let checked = check(text, cat);
    let mut findings = checked.findings;
    let cfg = checked.table.and_then(|t| match toml::Value::Table(t).try_into::<Config>() {
        Ok(cfg) => Some(cfg),
        // The checker passed something the typed config rejects: a catalogue
        // entry disagrees with its field.
        Err(e) => {
            findings.push(Finding {
                span: 0..0,
                severity: Severity::Error,
                message: format!("config doesn't match its catalogue: {e}"),
            });
            None
        }
    });
    (cfg, findings)
}

/// The config file's path, creating the file with [`default_file`] when absent.
pub fn ensure_file() -> Option<PathBuf> {
    let path = config_path()?;
    if !path.exists() {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, default_file());
    }
    Some(path)
}

/// What loading the config file found.
#[derive(Debug)]
pub struct Loaded {
    /// `None` when the file can't be read or isn't valid TOML: the caller
    /// keeps the config it has.
    pub config: Option<Config>,
    pub report: Report,
}

/// The problems a load found in the config file.
#[derive(Debug)]
pub struct Report {
    pub path: PathBuf,
    /// Why the file couldn't be read.
    pub read_error: Option<String>,
    /// Each finding: 1-based line and column (in chars), severity, message.
    pub findings: Vec<(usize, usize, Severity, String)>,
}

impl Report {
    /// Whether the load found anything to tell the user.
    pub fn has_problems(&self) -> bool {
        self.read_error.is_some() || !self.findings.is_empty()
    }

    /// One line per problem, for stderr.
    pub fn log_lines(&self) -> Vec<String> {
        let path = self.path.display();
        let read = self.read_error.iter().map(|e| format!("ghostrealm: can't read {path}: {e}"));
        let found = self.findings.iter().map(|(line, col, severity, message)| {
            let level = match severity {
                Severity::Error => "error",
                Severity::Warning => "warning",
            };
            format!("{path}:{line}:{col}: {level}: {message}")
        });
        read.chain(found).collect()
    }
}

/// Read the config file (see [`ensure_file`]). `None` when there is no
/// config path (no home directory).
pub fn load_or_create(cat: &Catalogue) -> Option<Loaded> {
    Some(load(ensure_file()?, cat))
}

/// Read and check the config file at `path`.
pub fn load(path: PathBuf, cat: &Catalogue) -> Loaded {
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) => {
            let report = Report { path, read_error: Some(e.to_string()), findings: Vec::new() };
            return Loaded { config: None, report };
        }
    };
    let (config, findings) = parse(&text, cat);
    let findings = findings
        .into_iter()
        .map(|f| {
            let (line, col) = line_col(&text, f.span.start);
            (line, col, f.severity, f.message)
        })
        .collect();
    Loaded { config, report: Report { path, read_error: None, findings } }
}

/// 1-based line and column (in chars) of byte `offset` in `text`.
fn line_col(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..offset.min(text.len())];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cat() -> Catalogue {
        let commands: Vec<KeyDoc> = default_binding_ids()
            .into_iter()
            .map(|key| KeyDoc { key, title: "", doc: "" })
            .collect();
        Catalogue::new(core_sections(&commands))
    }

    #[test]
    fn close_on_exit_takes_its_three_values_and_drops_the_rest() {
        let get = |v: &str| parse(&format!("[terminal]\nclose_on_exit = {v}\n"), &cat());
        assert_eq!(Terminal::default().close_on_exit, CloseOnExit::Clean);
        for (v, want) in [
            ("\"clean\"", CloseOnExit::Clean),
            ("\"always\"", CloseOnExit::Always),
            ("\"never\"", CloseOnExit::Never),
        ] {
            let (cfg, findings) = get(v);
            assert_eq!(cfg.unwrap().terminal.close_on_exit, want);
            assert!(findings.is_empty(), "{v}: {findings:?}");
        }
        let (cfg, findings) = get("\"sometimes\"");
        assert_eq!(cfg.unwrap().terminal.close_on_exit, CloseOnExit::Clean, "falls back");
        assert!(!findings.is_empty());
        assert!(CloseOnExit::Clean.closes(true) && !CloseOnExit::Clean.closes(false));
        assert!(CloseOnExit::Always.closes(false) && !CloseOnExit::Never.closes(true));
    }

    #[test]
    fn the_default_file_is_the_default_config() {
        let (cfg, findings) = parse(&default_file(), &cat());
        assert_eq!(findings, vec![]);
        assert_eq!(cfg, Some(Config::default()));
    }

    #[test]
    fn one_bad_entry_costs_only_that_entry() {
        let text = "[terminal]\nfont_size = \"big\"\nline_height = 20.0\n[nope]\n";
        let (cfg, findings) = parse(text, &cat());
        let cfg = cfg.unwrap();
        assert_eq!(findings.len(), 2);
        assert_eq!(cfg.terminal.font_size, Terminal::default().font_size);
        assert_eq!(cfg.terminal.line_height, 20.0);
    }

    #[test]
    fn a_syntax_error_yields_no_config() {
        assert_eq!(parse("[terminal\n", &cat()).0, None);
    }

    #[test]
    fn an_overflowing_number_yields_no_config() {
        // Not an empty table: that would unbind every key and reset every option.
        assert_eq!(parse("[terminal]\nfont_size = 1e400\n", &cat()).0, None);
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
    fn a_new_config_file_binds_the_comment_toggle_and_a_missing_section_binds_nothing() {
        assert!(default_file().contains("\"editor.toggle_comment\" = \"cmd+/\""));
        let (cfg, _) = parse("[tabs]\n", &cat());
        assert_eq!(cfg.unwrap().binding("cmd+/"), None, "defaults are file text, never implicit");
    }

    #[test]
    fn only_listed_commands_are_bound() {
        let cfg = Config::default();
        assert_eq!(cfg.binding("cmd+t").as_deref(), Some("surface.new"));
        assert_eq!(cfg.binding("cmd+j"), None);

        let (cfg, _) = parse("[keybindings]\n\"surface.new\" = \"Super+N\"\n", &cat());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.binding("cmd+n").as_deref(), Some("surface.new"));
        assert_eq!(cfg.binding_for("surface.new").as_deref(), Some("cmd+n"));
        assert_eq!(cfg.binding("cmd+t"), None);

        // No section: nothing bound.
        let (cfg, _) = parse("[tabs]\n", &cat());
        assert!(cfg.unwrap().keybindings.is_empty());
    }

    fn tmp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ghostrealm-config-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_load_reports_each_findings_line_column_and_severity() {
        let path = tmp("clamp").join("config.toml");
        std::fs::write(&path, "[terminal]\nfont_size = 1000\n").unwrap();
        let loaded = load(path.clone(), &cat());
        assert_eq!(loaded.config.unwrap().terminal.font_size, 96.0, "clamped and applied");
        let r = loaded.report;
        assert!(r.read_error.is_none());
        assert_eq!(r.findings.len(), 1, "{:?}", r.findings);
        let (line, col, severity, message) = &r.findings[0];
        assert_eq!((*line, *severity), (2, Severity::Warning));
        assert!(*col > 1 && message.contains("clamped"), "{col} {message}");
        assert!(r.has_problems());
        assert_eq!(r.log_lines(), [format!("{}:2:{col}: warning: {message}", path.display())]);
    }

    #[test]
    fn an_unreadable_file_is_reported_and_not_applied() {
        let loaded = load(tmp("unreadable"), &cat());
        assert!(loaded.config.is_none());
        assert!(loaded.report.read_error.is_some());
        assert!(loaded.report.log_lines()[0].starts_with("ghostrealm: can't read "));
    }

    #[test]
    fn a_clean_load_has_no_problems() {
        let path = tmp("clean").join("config.toml");
        std::fs::write(&path, "[terminal]\nfont_size = 14\n").unwrap();
        let loaded = load(path, &cat());
        assert!(loaded.config.is_some());
        assert!(!loaded.report.has_problems());
    }
}
