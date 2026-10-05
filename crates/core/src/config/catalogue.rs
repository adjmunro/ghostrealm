//! The option catalogue: every config section and option with its type,
//! constraints, default, and description. It is the single source for the
//! checker (shared by the loader and the config language server) and for hover
//! docs, so the config file itself only needs the options a user sets.
//!
//! The app's own sections come from [`core_sections`]; plugins add theirs, so a
//! section's docs live beside the code that reads it.

/// The type of an option's value, with its constraints.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Bool,
    /// An integer in `min..=max`. Values outside are clamped.
    Int { min: i64, max: i64 },
    /// A number (integer or float) in `min..=max`. Values outside are clamped.
    Float { min: f64, max: f64 },
    Str,
    /// A filesystem path; a leading `~` expands to `$HOME`.
    Path,
    /// One of these strings, each with a description.
    Enum(&'static [(&'static str, &'static str)]),
    /// `[r, g, b]`, each component `0..=255`.
    Rgb,
    /// A key chord such as `"cmd+shift+d"`: modifiers in any order or alias
    /// (`cmd`/`super`/`meta`, `opt`/`alt`) plus one key.
    Chord,
}

impl Kind {
    /// A one-line type description: `bool`, `integer 0–255`, `"a" | "b"`, ...
    pub fn describe(&self) -> String {
        match self {
            Kind::Bool => "bool".into(),
            Kind::Int { min, max } => format!("integer {min}–{max}"),
            Kind::Float { min, max } => format!("number {min}–{max}"),
            Kind::Str => "string".into(),
            Kind::Path => "path (`~` expands)".into(),
            Kind::Enum(variants) => variants
                .iter()
                .map(|(v, _)| format!("\"{v}\""))
                .collect::<Vec<_>>()
                .join(" | "),
            Kind::Rgb => "[r, g, b], each 0–255".into(),
            Kind::Chord => "chord, e.g. \"cmd+shift+d\"".into(),
        }
    }
}

/// One option in a section.
#[derive(Clone, Debug, PartialEq)]
pub struct OptionSpec {
    pub key: &'static str,
    pub kind: Kind,
    /// The default as a TOML literal, or `None` when unset by default.
    pub default: Option<&'static str>,
    pub doc: &'static str,
}

/// A key a [`Entries::Map`] section accepts, with its name and description.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyDoc {
    pub key: &'static str,
    pub title: &'static str,
    pub doc: &'static str,
}

impl KeyDoc {
    pub fn hover(&self) -> String {
        if self.doc.is_empty() {
            format!("{}: {}", self.key, self.title)
        } else {
            format!("{}: {}\n{}", self.key, self.title, self.doc)
        }
    }
}

/// What a section holds.
#[derive(Clone, Debug, PartialEq)]
pub enum Entries {
    /// A fixed set of options.
    Options(Vec<OptionSpec>),
    /// Keys from a known set (e.g. command ids), each holding a `value`.
    Map { keys: Vec<KeyDoc>, value: Kind },
}

/// A top-level `[section]`.
#[derive(Clone, Debug, PartialEq)]
pub struct SectionSpec {
    pub name: &'static str,
    pub doc: &'static str,
    pub entries: Entries,
}

impl SectionSpec {
    pub fn options(name: &'static str, doc: &'static str, options: Vec<OptionSpec>) -> Self {
        SectionSpec {
            name,
            doc,
            entries: Entries::Options(options),
        }
    }

    /// The option named `key`, for an [`Entries::Options`] section.
    pub fn option(&self, key: &str) -> Option<&OptionSpec> {
        match &self.entries {
            Entries::Options(opts) => opts.iter().find(|o| o.key == key),
            Entries::Map { .. } => None,
        }
    }

    /// Hover text for the section header: its description and every entry.
    pub fn hover(&self) -> String {
        let mut s = format!("[{}]\n{}\n", self.name, self.doc);
        match &self.entries {
            Entries::Options(opts) => {
                for o in opts {
                    let default = o.default.unwrap_or("unset");
                    s.push_str(&format!("\n{} = {default}  ({})", o.key, o.kind.describe()));
                }
            }
            Entries::Map { keys, value } => {
                s.push_str(&format!("\nEach entry: \"key\" = {}", value.describe()));
                for k in keys {
                    s.push_str(&format!("\n{}  ({})", k.key, k.title));
                }
            }
        }
        s
    }
}

impl OptionSpec {
    pub const fn new(key: &'static str, kind: Kind, default: Option<&'static str>, doc: &'static str) -> Self {
        OptionSpec { key, kind, default, doc }
    }

    /// Hover text: the option's path and type, description, allowed values, and
    /// default.
    pub fn hover(&self, section: &str) -> String {
        let mut s = format!("[{section}] {}: {}\n{}", self.key, self.kind.describe(), self.doc);
        if let Kind::Enum(variants) = self.kind {
            s.push('\n');
            for (v, doc) in variants {
                s.push_str(&format!("\n\"{v}\": {doc}"));
            }
        }
        s.push_str(&format!("\n\nDefault: {}", self.default.unwrap_or("unset")));
        s
    }
}

/// Every known section: the app's own plus any plugins contributed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Catalogue {
    sections: Vec<SectionSpec>,
}

impl Catalogue {
    pub fn new(sections: Vec<SectionSpec>) -> Self {
        Catalogue { sections }
    }

    pub fn section(&self, name: &str) -> Option<&SectionSpec> {
        self.sections.iter().find(|s| s.name == name)
    }

    pub fn sections(&self) -> &[SectionSpec] {
        &self.sections
    }
}

const SIDES: &[(&str, &str)] = &[("left", "Left edge of the window."), ("right", "Right edge of the window.")];

/// The app's own sections. `commands` lists every command a chord can bind:
/// the keys `[keybindings]` accepts.
pub fn core_sections(commands: &[KeyDoc]) -> Vec<SectionSpec> {
    use Kind::*;
    let mut keys = commands.to_vec();
    keys.sort_by_key(|k| k.key);
    keys.dedup_by_key(|k| k.key);
    vec![
        SectionSpec::options(
            "workspace",
            "Workspaces: the named entries in the sidebar.",
            vec![OptionSpec::new(
                "default_directory",
                Path,
                None,
                "Directory new workspaces and terminals start in when the workspace has no \
                 pinned root. Unset means $HOME.",
            )],
        ),
        SectionSpec::options(
            "sidebar",
            "The workspace sidebar.",
            vec![
                OptionSpec::new("side", Enum(SIDES), Some("\"left\""), "Which edge the sidebar sits on."),
                OptionSpec::new(
                    "width",
                    Float { min: 40.0, max: 1000.0 },
                    Some("190.0"),
                    "Sidebar width in logical pixels.",
                ),
            ],
        ),
        SectionSpec::options(
            "terminal",
            "Text metrics for terminals and every other monospace view.",
            vec![
                OptionSpec::new("font_size", Float { min: 4.0, max: 96.0 }, Some("15.0"), "Font size in points."),
                OptionSpec::new(
                    "line_height",
                    Float { min: 4.0, max: 200.0 },
                    Some("18.0"),
                    "Row height in points (at least font_size).",
                ),
            ],
        ),
        SectionSpec::options(
            "chrome",
            "Colours for the app's own UI (not terminal contents). Without this section, \
             colours come from your Ghostty config, falling back to the defaults below. \
             With it, unset colours use those defaults.",
            vec![
                OptionSpec::new("background", Rgb, Some("[20, 20, 24]"), "Behind panes and split dividers."),
                OptionSpec::new("sidebar", Rgb, Some("[24, 24, 30]"), "Sidebar and ribbon background."),
                OptionSpec::new(
                    "accent",
                    Rgb,
                    Some("[90, 140, 220]"),
                    "Focused-pane border, selections, and the palette's selected row.",
                ),
            ],
        ),
        SectionSpec::options(
            "tabs",
            "A pane's horizontal tab strip.",
            vec![OptionSpec::new(
                "autohide_single_tab",
                Bool,
                Some("true"),
                "Hide the tab strip while a pane holds only one tab.",
            )],
        ),
        SectionSpec::options(
            "input",
            "Mouse and keyboard behaviour.",
            vec![
                OptionSpec::new("focus_follows_mouse", Bool, Some("false"), "Hovering a pane focuses it."),
                OptionSpec::new(
                    "double_tap_window_ms",
                    Int { min: 0, max: 2000 },
                    Some("300"),
                    "Longest gap (ms) between the two taps of a double-tap: Shift opens the \
                     palette, Ctrl opens Run.",
                ),
                OptionSpec::new(
                    "double_click_ms",
                    Int { min: 0, max: 2000 },
                    Some("200"),
                    "Longest gap (ms) between the two clicks of a double-click.",
                ),
            ],
        ),
        SectionSpec::options(
            "inbox",
            "When a workspace's unread status clears.",
            vec![
                OptionSpec::new(
                    "auto_read_after",
                    Int { min: 0, max: 3600 },
                    Some("3"),
                    "Seconds a workspace must stay focused before it is marked read (0 = only \
                     by hand).",
                ),
                OptionSpec::new(
                    "auto_unread_before",
                    Int { min: 0, max: 3600 },
                    Some("1"),
                    "Leaving a workspace within this many seconds of it being auto-read marks \
                     it unread again.",
                ),
            ],
        ),
        SectionSpec {
            name: "keybindings",
            doc: "\"command id\" = \"chord\". Only the commands listed here are bound. Cmd \
                  chords only; other keys go to the focused view.",
            entries: Entries::Map { keys, value: Chord },
        },
    ]
}
