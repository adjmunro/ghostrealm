//! Best-effort inheritance of chrome colours from the user's Ghostty config.
//!
//! When ghostrealm's `[chrome]` is unset, we colour our own UI from Ghostty's
//! `background` / `foreground` (and a cursor/selection/palette accent) so the app
//! blends with the user's terminal theme. libghostty-vt does not expose the
//! resolved config, so we parse the file ourselves — directly-set keys only; a
//! theme-based config (no explicit `background`) yields nothing and we fall back
//! to the built-in defaults.

use std::path::PathBuf;

use crate::config::Chrome;

/// Derive chrome colours from the user's Ghostty config, if one is found and sets
/// at least a `background`.
pub fn inherited_chrome() -> Option<Chrome> {
    let text = std::fs::read_to_string(ghostty_config_path()?).ok()?;
    derive_chrome(&parse_colors(&text))
}

/// Colours pulled from a Ghostty config.
#[derive(Default, Debug, PartialEq, Eq)]
struct GhosttyColors {
    background: Option<[u8; 3]>,
    foreground: Option<[u8; 3]>,
    cursor: Option<[u8; 3]>,
    selection: Option<[u8; 3]>,
    /// palette index -> colour, for a small number of indices we care about.
    palette: Vec<(u8, [u8; 3])>,
}

impl GhosttyColors {
    fn palette(&self, idx: u8) -> Option<[u8; 3]> {
        self.palette.iter().find(|(i, _)| *i == idx).map(|(_, c)| *c)
    }
}

fn derive_chrome(c: &GhosttyColors) -> Option<Chrome> {
    let bg = c.background?;
    let fg = c.foreground.unwrap_or([220, 220, 230]);
    let accent = c
        .cursor
        .or(c.selection)
        .or_else(|| c.palette(4))
        .unwrap_or([90, 140, 220]);
    Some(Chrome {
        background: bg,
        // Nudge the sidebar a touch toward the foreground so it reads as a
        // distinct panel against the pane background.
        sidebar: mix(bg, fg, 0.06),
        accent,
    })
}

/// Linear per-channel blend: `a` moved `t` (0..=1) of the way toward `b`.
fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> [u8; 3] {
    let t = t.clamp(0.0, 1.0);
    let ch = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    [ch(a[0], b[0]), ch(a[1], b[1]), ch(a[2], b[2])]
}

/// Parse the colour-relevant keys from a Ghostty config (`key = value`, `#`
/// comments, last assignment wins; `palette = N=#hex`).
fn parse_colors(text: &str) -> GhosttyColors {
    let mut c = GhosttyColors::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "background" => c.background = parse_color(value),
            "foreground" => c.foreground = parse_color(value),
            "cursor-color" => c.cursor = parse_color(value),
            "selection-background" => c.selection = parse_color(value),
            "palette" => {
                // value is "N=#rrggbb"
                if let Some((idx, col)) = value.split_once('=') {
                    if let (Ok(idx), Some(col)) = (idx.trim().parse::<u8>(), parse_color(col.trim()))
                    {
                        c.palette.retain(|(i, _)| *i != idx);
                        c.palette.push((idx, col));
                    }
                }
            }
            _ => {}
        }
    }
    c
}

/// Parse a Ghostty colour: `#rrggbb` or `rrggbb` (case-insensitive). Named colours
/// are not resolved (returns `None`).
fn parse_color(s: &str) -> Option<[u8; 3]> {
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
    Some([byte(0)?, byte(2)?, byte(4)?])
}

/// The Ghostty config path: `$XDG_CONFIG_HOME/ghostty/config` (falling back to
/// `~/.config/ghostty/config`), else the macOS app-support location. Returns the
/// first that exists.
fn ghostty_config_path() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(base) = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
    {
        candidates.push(base.join("ghostty").join("config"));
    }
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        candidates.push(
            home.join("Library")
                .join("Application Support")
                .join("com.mitchellh.ghostty")
                .join("config"),
        );
    }
    candidates.into_iter().find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_colours_with_and_without_hash() {
        assert_eq!(parse_color("#1e1e2e"), Some([30, 30, 46]));
        assert_eq!(parse_color("1E1E2E"), Some([30, 30, 46]));
        assert_eq!(parse_color("#fff"), None, "short hex is not supported");
        assert_eq!(parse_color("rebeccapurple"), None, "named colours are skipped");
    }

    #[test]
    fn derives_chrome_from_background_and_accent() {
        let cfg = "\
# my ghostty theme
background = 1e1e2e
foreground = #cdd6f4
cursor-color = f5e0dc
palette = 4=#89b4fa
";
        let chrome = derive_chrome(&parse_colors(cfg)).expect("background present");
        assert_eq!(chrome.background, [30, 30, 46]);
        assert_eq!(chrome.accent, [245, 224, 220], "cursor-color wins the accent");
        // Sidebar is nudged from the background toward the foreground.
        assert_ne!(chrome.sidebar, chrome.background);
        assert!(chrome.sidebar[0] >= chrome.background[0]);
    }

    #[test]
    fn accent_falls_back_to_selection_then_palette() {
        let cfg = "background = 000000\nselection-background = 112233\n";
        let chrome = derive_chrome(&parse_colors(cfg)).unwrap();
        assert_eq!(chrome.accent, [17, 34, 51], "selection is the next accent choice");

        let cfg = "background = 000000\npalette = 4=445566\n";
        let chrome = derive_chrome(&parse_colors(cfg)).unwrap();
        assert_eq!(chrome.accent, [68, 85, 102], "then palette index 4");
    }

    #[test]
    fn no_background_means_no_inheritance() {
        // A theme-only config (no explicit background) yields nothing.
        let cfg = "theme = catppuccin-mocha\nfont-size = 14\n";
        assert!(derive_chrome(&parse_colors(cfg)).is_none());
    }

    #[test]
    fn last_assignment_wins() {
        let cfg = "background = 111111\nbackground = 222222\n";
        assert_eq!(parse_colors(cfg).background, Some([34, 34, 34]));
    }
}
