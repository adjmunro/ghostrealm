//! Checking config text against the [`Catalogue`]: syntax errors, unknown
//! sections/options/commands, wrong types, out-of-range values, and duplicate
//! chords. The loader applies the cleaned-up table (one bad entry costs only
//! that entry); the config language server shows the findings in the editor.

use std::ops::Range;

use serde::de::{Deserialize, IntoDeserializer};
use toml::de::{DeTable, DeValue};
use toml::Value;

use super::catalogue::{Catalogue, Entries, Kind};
use super::normalize_chord;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The entry is ignored (or the file doesn't parse).
    Error,
    /// The entry applies, adjusted (e.g. clamped into range).
    Warning,
}

/// One problem in the text, located by byte span.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub span: Range<usize>,
    pub severity: Severity,
    pub message: String,
}

impl Finding {
    fn error(span: Range<usize>, message: impl Into<String>) -> Self {
        Finding { span, severity: Severity::Error, message: message.into() }
    }

    fn warning(span: Range<usize>, message: impl Into<String>) -> Self {
        Finding { span, severity: Severity::Warning, message: message.into() }
    }
}

/// The result of [`check`].
#[derive(Debug)]
pub struct Checked {
    pub findings: Vec<Finding>,
    /// The parsed config with every flagged entry dropped or adjusted, ready to
    /// deserialize. `None` when the text isn't valid TOML.
    pub table: Option<toml::Table>,
}

/// What to do with one value.
enum Verdict {
    Ok,
    /// Use this value instead, reporting the message (if any) as a warning.
    Replace(Value, Option<String>),
    /// Drop the entry (its default applies), reporting the message as an error.
    Invalid(String),
}

/// Check `text` against `cat`.
pub fn check(text: &str, cat: &Catalogue) -> Checked {
    let (doc, errors) = DeTable::parse_recoverable(text);
    let mut findings: Vec<Finding> = errors
        .iter()
        .map(|e| Finding::error(e.span().unwrap_or(0..0), e.message()))
        .collect();
    let mut table = toml::Table::deserialize(doc.clone().into_deserializer()).unwrap_or_default();
    // Recovery fills a broken value with a placeholder; checking it would
    // report the syntax error twice.
    let broken = |r: &Range<usize>| {
        findings_spans(&errors).any(|e| e.start <= r.end && r.start <= e.end)
    };

    for (key, value) in doc.get_ref() {
        let name: &str = key.get_ref();
        let Some(spec) = cat.section(name) else {
            findings.push(Finding::error(key.span(), format!("unknown section `{name}`")));
            table.remove(name);
            continue;
        };
        let DeValue::Table(entries) = value.get_ref() else {
            findings.push(Finding::error(
                key.span(),
                format!("`{name}` must be a table: write it as `[{name}]`"),
            ));
            table.remove(name);
            continue;
        };
        let Some(Value::Table(values)) = table.get_mut(name) else {
            continue;
        };
        // File order, so "first binding wins" means first in the file.
        let mut entries: Vec<_> = entries.iter().collect();
        entries.sort_by_key(|(k, _)| k.span().start);
        let mut chords: Vec<(String, &str)> = Vec::new();
        for (k, v) in entries {
            let key: &str = k.get_ref();
            let kind = match &spec.entries {
                Entries::Options(_) => match spec.option(key) {
                    Some(opt) => opt.kind,
                    None => {
                        findings.push(Finding::error(
                            k.span(),
                            format!("unknown option `{key}` in [{name}]"),
                        ));
                        values.remove(key);
                        continue;
                    }
                },
                Entries::Map { keys, value } => {
                    if !keys.iter().any(|d| d.key == key) {
                        findings.push(Finding::error(
                            k.span(),
                            format!("unknown command `{key}`; this binding is ignored"),
                        ));
                        values.remove(key);
                        continue;
                    }
                    *value
                }
            };
            let verdict = match values.get(key) {
                Some(found) if !broken(&v.span()) => check_value(key, kind, found),
                _ => Verdict::Ok,
            };
            match verdict {
                Verdict::Ok => {}
                Verdict::Replace(new, message) => {
                    if let Some(m) = message {
                        findings.push(Finding::warning(v.span(), m));
                    }
                    values.insert(key.to_string(), new);
                }
                Verdict::Invalid(m) => {
                    findings.push(Finding::error(v.span(), m));
                    values.remove(key);
                    continue;
                }
            }
            if kind == Kind::Chord {
                let chord = values.get(key).and_then(Value::as_str).and_then(normalize_chord);
                if let Some(chord) = chord {
                    if let Some((_, first)) = chords.iter().find(|(c, _)| *c == chord) {
                        findings.push(Finding::warning(
                            v.span(),
                            format!("`{chord}` is already bound to `{first}`; this binding is ignored"),
                        ));
                        values.remove(key);
                    } else {
                        chords.push((chord, key));
                    }
                }
            }
        }
    }

    findings.sort_by_key(|f| f.span.start);
    Checked {
        findings,
        table: errors.is_empty().then_some(table),
    }
}

fn findings_spans(errors: &[toml::de::Error]) -> impl Iterator<Item = Range<usize>> + '_ {
    errors.iter().filter_map(|e| e.span())
}

fn check_value(key: &str, kind: Kind, found: &Value) -> Verdict {
    let mismatch = || {
        Verdict::Invalid(format!(
            "`{key}` expects {}, found {}; the default applies",
            kind.describe(),
            found.type_str()
        ))
    };
    match kind {
        Kind::Bool => match found {
            Value::Boolean(_) => Verdict::Ok,
            _ => mismatch(),
        },
        Kind::Str | Kind::Path => match found {
            Value::String(_) => Verdict::Ok,
            _ => mismatch(),
        },
        Kind::Int { min, max } => match found {
            Value::Integer(i) if (min..=max).contains(i) => Verdict::Ok,
            Value::Integer(i) => {
                let c = (*i).clamp(min, max);
                Verdict::Replace(
                    Value::Integer(c),
                    Some(format!("`{key}` must be {min}–{max}; clamped to {c}")),
                )
            }
            _ => mismatch(),
        },
        Kind::Float { min, max } => {
            let f = match found {
                Value::Float(f) if f.is_finite() => *f,
                Value::Integer(i) => *i as f64,
                _ => return mismatch(),
            };
            if (min..=max).contains(&f) {
                // Integers are valid numbers here; store them as floats so the
                // typed field deserializes.
                Verdict::Replace(Value::Float(f), None)
            } else {
                let c = f.clamp(min, max);
                Verdict::Replace(
                    Value::Float(c),
                    Some(format!("`{key}` must be {min}–{max}; clamped to {c}")),
                )
            }
        }
        Kind::Enum(variants) => match found {
            Value::String(s) if variants.iter().any(|(v, _)| v == s) => Verdict::Ok,
            Value::String(s) => Verdict::Invalid(format!(
                "`{key}` must be {}, not \"{s}\"; the default applies",
                kind.describe()
            )),
            _ => mismatch(),
        },
        Kind::Rgb => {
            let Value::Array(items) = found else {
                return mismatch();
            };
            let ints: Option<Vec<i64>> = items.iter().map(Value::as_integer).collect();
            match ints {
                Some(c) if c.len() == 3 => {
                    if c.iter().all(|v| (0..=255).contains(v)) {
                        Verdict::Ok
                    } else {
                        let clamped: Vec<Value> =
                            c.iter().map(|v| Value::Integer((*v).clamp(0, 255))).collect();
                        Verdict::Replace(
                            Value::Array(clamped),
                            Some(format!("`{key}` components must be 0–255; clamped")),
                        )
                    }
                }
                _ => mismatch(),
            }
        }
        Kind::Chord => match found {
            Value::String(s) if normalize_chord(s).is_some() => Verdict::Ok,
            Value::String(s) => Verdict::Invalid(format!(
                "\"{s}\" is not a chord (modifiers plus one key); this binding is ignored"
            )),
            _ => mismatch(),
        },
    }
}

/// Docs for whatever sits at byte `offset` of `text`: a section header, an
/// option or command key, or a value. Returns the span it covers and the text.
pub fn hover_at(text: &str, cat: &Catalogue, offset: usize) -> Option<(Range<usize>, String)> {
    let (doc, _) = DeTable::parse_recoverable(text);
    let inside = |r: &Range<usize>| r.start <= offset && offset < r.end;
    for (key, value) in doc.get_ref() {
        let name: &str = key.get_ref();
        let spec = cat.section(name);
        if inside(&key.span()) {
            return spec.map(|s| (key.span(), s.hover()));
        }
        let (Some(spec), DeValue::Table(entries)) = (spec, value.get_ref()) else {
            continue;
        };
        for (k, v) in entries {
            let span = if inside(&k.span()) {
                k.span()
            } else if inside(&v.span()) {
                v.span()
            } else {
                continue;
            };
            let key: &str = k.get_ref();
            return match &spec.entries {
                Entries::Options(_) => spec.option(key).map(|o| (span, o.hover(name))),
                Entries::Map { keys, value } => keys.iter().find(|d| d.key == key).map(|d| {
                    (span, format!("{}\n\n[{name}] value: {}", d.hover(), value.describe()))
                }),
            };
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::catalogue::{core_sections, KeyDoc};

    fn cat() -> Catalogue {
        Catalogue::new(core_sections(&[
            KeyDoc { key: "surface.new", title: "New Terminal Tab", doc: "" },
            KeyDoc { key: "pane.close", title: "Close Pane", doc: "" },
        ]))
    }

    fn messages(text: &str) -> Vec<(Severity, String, String)> {
        check(text, &cat())
            .findings
            .into_iter()
            .map(|f| (f.severity, text[f.span].to_string(), f.message))
            .collect()
    }

    #[test]
    fn a_valid_file_has_no_findings() {
        let text = "[sidebar]\nside = \"right\"\nwidth = 200\n\n[keybindings]\n\"surface.new\" = \"cmd+t\"\n";
        assert_eq!(messages(text), vec![]);
        let table = check(text, &cat()).table.unwrap();
        // An integer for a number option is stored as a float.
        assert_eq!(table["sidebar"]["width"], Value::Float(200.0));
    }

    #[test]
    fn unknown_sections_options_and_commands_are_errors_and_dropped() {
        let text = "[bogus]\nx = 1\n\n[sidebar]\nwdith = 3\n\n[keybindings]\n\"nope\" = \"cmd+j\"\n";
        let found = messages(text);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found.iter().all(|(s, _, _)| *s == Severity::Error));
        assert_eq!(found[0].1, "bogus");
        assert_eq!(found[1].1, "wdith");
        assert_eq!(found[2].1, "\"nope\"");
        let table = check(text, &cat()).table.unwrap();
        assert!(!table.contains_key("bogus"));
        assert!(table["sidebar"].as_table().unwrap().is_empty());
        assert!(table["keybindings"].as_table().unwrap().is_empty());
    }

    #[test]
    fn wrong_types_are_dropped_so_the_default_applies() {
        let text = "[tabs]\nautohide_single_tab = \"yes\"\n[sidebar]\nside = \"top\"\n";
        let found = messages(text);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(found[0].1, "\"yes\"");
        assert!(found[0].2.contains("expects bool, found string"));
        assert_eq!(found[1].1, "\"top\"");
        let table = check(text, &cat()).table.unwrap();
        assert!(table["tabs"].get("autohide_single_tab").is_none());
        assert!(table["sidebar"].get("side").is_none());
    }

    #[test]
    fn out_of_range_values_are_clamped_with_a_warning() {
        let text = "[terminal]\nfont_size = 1000\n[input]\ndouble_click_ms = -5\n[chrome]\naccent = [300, 0, 0]\n";
        let found = messages(text);
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(found.iter().all(|(s, _, _)| *s == Severity::Warning));
        let table = check(text, &cat()).table.unwrap();
        assert_eq!(table["terminal"]["font_size"], Value::Float(96.0));
        assert_eq!(table["input"]["double_click_ms"], Value::Integer(0));
        assert_eq!(table["chrome"]["accent"][0], Value::Integer(255));
    }

    #[test]
    fn bad_and_duplicate_chords() {
        let text = "[keybindings]\n\"surface.new\" = \"cmd+t\"\n\"pane.close\" = \"Super+T\"\n";
        let found = messages(text);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].1, "\"Super+T\"");
        assert!(found[0].2.contains("already bound to `surface.new`"));
        let table = check(text, &cat()).table.unwrap();
        assert!(table["keybindings"].get("pane.close").is_none());

        let found = messages("[keybindings]\n\"surface.new\" = \"cmd+t+w\"\n");
        assert_eq!(found[0].0, Severity::Error);
        assert!(found[0].2.contains("not a chord"));
    }

    #[test]
    fn a_syntax_error_has_a_span_and_no_table() {
        let text = "[sidebar]\nside = \n";
        let checked = check(text, &cat());
        assert!(checked.table.is_none());
        assert_eq!(checked.findings.len(), 1);
        assert_eq!(checked.findings[0].severity, Severity::Error);
    }

    #[test]
    fn a_section_that_is_not_a_table() {
        let found = messages("sidebar = 3\n");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].1, "sidebar");
    }

    #[test]
    fn hover_finds_sections_options_values_and_commands() {
        let text = "[editor]\n[sidebar]\nside = \"left\"\n[keybindings]\n\"surface.new\" = \"cmd+t\"\n";
        let at = |needle: &str| {
            let off = text.find(needle).unwrap() + 1;
            hover_at(text, &cat(), off).map(|(span, s)| (text[span].to_string(), s))
        };
        let (span, doc) = at("sidebar]").unwrap();
        assert_eq!(span, "sidebar");
        assert!(doc.contains("width = 190.0"), "{doc}");
        let (span, doc) = at("side =").unwrap();
        assert_eq!(span, "side");
        assert!(doc.contains("\"left\" | \"right\""), "{doc}");
        assert!(doc.contains("Default: \"left\""), "{doc}");
        let (span, _) = at("\"left\"").unwrap();
        assert_eq!(span, "\"left\"");
        let (_, doc) = at("\"surface.new\"").unwrap();
        assert!(doc.starts_with("surface.new: New Terminal Tab"), "{doc}");
        // Not in this catalogue: no docs.
        assert!(at("editor]").is_none());
    }
}
