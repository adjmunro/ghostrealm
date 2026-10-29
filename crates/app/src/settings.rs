//! The config file as the settings UI: the option catalogue it is checked and
//! documented against (the app's sections plus each plugin's), and the
//! language server that brings both into the editor.

use std::sync::Arc;

use ghostrealm_core::config::{check, core_sections, hover_at, Catalogue, KeyDoc, Severity};
use ghostrealm_core::Registry;
use ghostrealm_lsp::types::{
    Diagnostic, DiagnosticSeverity, Hover, HoverContents, HoverProviderCapability, MarkupContent,
    MarkupKind, Position, ServerCapabilities,
};
use ghostrealm_lsp::{Document, LanguageServer};

use crate::app_state::AppState;

/// UI actions that are keybindable but aren't registry commands (they act on
/// window state, not `AppState`). Dispatched specially in the window's key
/// handling.
pub const SPECIAL_COMMANDS: &[KeyDoc] = &[KeyDoc {
    key: "palette.toggle",
    title: "Command Palette",
    doc: "Open or close the command palette.",
}];

/// Every command a chord can bind: the non-hidden registry commands plus the
/// special UI actions.
pub fn keybindable_commands(registry: &Registry<AppState>) -> Vec<KeyDoc> {
    let mut cmds: Vec<KeyDoc> = registry
        .metas()
        .filter(|m| !m.hidden)
        .map(|m| KeyDoc { key: m.id, title: m.title, doc: m.description })
        .collect();
    cmds.extend_from_slice(SPECIAL_COMMANDS);
    cmds
}

/// The full option catalogue: the app's sections, `[keybindings]` keyed by
/// `registry`'s commands, and every built-in plugin's sections.
pub fn catalogue(registry: &Registry<AppState>) -> Catalogue {
    let mut sections = core_sections(&keybindable_commands(registry));
    for plugin in crate::plugins::builtin() {
        sections.extend(plugin.config_sections());
    }
    Catalogue::new(sections)
}

/// The config file's language server: the checker's findings as diagnostics,
/// and the catalogue's docs on hover.
pub struct ConfigServer {
    catalogue: Arc<Catalogue>,
}

impl ConfigServer {
    pub fn new(catalogue: Arc<Catalogue>) -> Self {
        ConfigServer { catalogue }
    }
}

impl LanguageServer for ConfigServer {
    fn name(&self) -> &str {
        "ghostrealm-config"
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        }
    }

    fn diagnostics(&mut self, doc: &Document) -> Vec<Diagnostic> {
        check(&doc.text, &self.catalogue)
            .findings
            .into_iter()
            .map(|f| Diagnostic {
                range: doc.range(f.span),
                severity: Some(match f.severity {
                    Severity::Error => DiagnosticSeverity::ERROR,
                    Severity::Warning => DiagnosticSeverity::WARNING,
                }),
                source: Some("ghostrealm".into()),
                message: f.message,
                ..Default::default()
            })
            .collect()
    }

    fn hover(&mut self, doc: &Document, pos: Position) -> Option<Hover> {
        let (span, text) = hover_at(&doc.text, &self.catalogue, doc.offset(pos)?)?;
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent { kind: MarkupKind::PlainText, value: text }),
            range: Some(doc.range(span)),
        })
    }
}

/// `ghostrealm lsp <name>`: serve a first-party language server on stdio, for
/// any LSP client (another editor, or a test).
pub fn serve_stdio(name: &str) -> anyhow::Result<()> {
    let mut server = match name {
        "config" => ConfigServer::new(Arc::new(catalogue(&crate::app_state::build_registry()))),
        other => anyhow::bail!("no language server `{other}` (available: config)"),
    };
    let (conn, io) = ghostrealm_lsp::Connection::stdio();
    ghostrealm_lsp::serve(&mut server, &conn).map_err(|e| anyhow::anyhow!("{e}"))?;
    drop(conn);
    io.join()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use ghostrealm_core::config::{default_file, parse, Entries};
    use ghostrealm_core::Config;

    use crate::app_state::build_registry;

    /// A config file setting every catalogued option to its documented default.
    fn every_default(cat: &ghostrealm_core::config::Catalogue, skip: &[&str]) -> String {
        let mut text = String::new();
        for section in cat.sections() {
            let Entries::Options(opts) = &section.entries else {
                continue;
            };
            if skip.contains(&section.name) {
                continue;
            }
            text.push_str(&format!("[{}]\n", section.name));
            for o in opts {
                if let Some(d) = o.default {
                    text.push_str(&format!("{} = {d}\n", o.key));
                }
            }
        }
        text
    }

    #[test]
    fn documented_defaults_are_the_real_defaults() {
        let cat = super::catalogue(&build_registry());
        // `[chrome]` is unset by default (inherit from Ghostty); its documented
        // defaults are what an explicit section starts from.
        let text = every_default(&cat, &["chrome"]) + &default_file();
        let (cfg, findings) = parse(&text, &cat);
        assert_eq!(findings, vec![], "{text}");
        assert_eq!(cfg, Some(Config::default()));

        let (cfg, findings) = parse(&every_default(&cat, &[]), &cat);
        assert_eq!(findings, vec![]);
        assert_eq!(cfg.unwrap().chrome, Some(ghostrealm_core::Chrome::default()));
    }

    #[test]
    fn every_option_is_catalogued() {
        let cat = super::catalogue(&build_registry());
        let mut cfg = Config { chrome: Some(Default::default()), ..Default::default() };
        cfg.workspace.default_directory = Some("~".into());
        cfg.editor.preview_font = Some("Georgia".into());
        let toml::Value::Table(all) = toml::Value::try_from(&cfg).unwrap() else {
            unreachable!()
        };
        for (name, section) in all {
            let spec = cat.section(&name).unwrap_or_else(|| panic!("[{name}] isn't catalogued"));
            if let Entries::Options(_) = spec.entries {
                for key in section.as_table().unwrap().keys() {
                    assert!(spec.option(key).is_some(), "[{name}] {key} isn't catalogued");
                }
            }
        }
    }

    #[test]
    fn the_preview_font_parses_and_is_checked() {
        let cat = super::catalogue(&build_registry());
        let (cfg, findings) = parse("[editor]\npreview_font = \"Iowan Old Style\"\n", &cat);
        assert_eq!(findings, vec![]);
        assert_eq!(cfg.unwrap().editor.preview_font.as_deref(), Some("Iowan Old Style"));
        let (cfg, findings) = parse("[editor]\npreview_font = 3\n", &cat);
        assert_eq!(findings.len(), 1, "not a string");
        assert_eq!(cfg.unwrap().editor.preview_font, None, "dropped");
    }

    #[test]
    fn heading_sizes_parse_and_are_checked() {
        let cat = super::catalogue(&build_registry());
        let text = "[headings]\nh2_scale = 1.7\nh1_space = 2\nh3_scale = 9.0\nh4_space = \"big\"\n";
        let (cfg, findings) = parse(text, &cat);
        let h = cfg.unwrap().headings;
        assert_eq!((h.h2_scale, h.h1_space), (1.7, 2.0), "an integer is a number");
        assert_eq!(h.h3_scale, 4.0, "clamped");
        assert_eq!(h.h4_space, ghostrealm_core::Headings::default().h4_space, "dropped");
        let severities: Vec<_> = findings.iter().map(|f| f.severity).collect();
        assert_eq!(severities, [ghostrealm_core::config::Severity::Warning, ghostrealm_core::config::Severity::Error]);
        let (cfg, _) = parse("[headings]\nh5_scale = 0.1\n", &cat);
        assert_eq!(cfg.unwrap().headings.h5_scale, 0.5, "never smaller than half of body text");
    }

    #[test]
    fn every_default_binding_is_a_listed_command() {
        let reg = build_registry();
        let listed: Vec<&str> = super::keybindable_commands(&reg).iter().map(|k| k.key).collect();
        for id in ghostrealm_core::config::default_binding_ids() {
            assert!(listed.contains(&id), "default binding for `{id}` isn't a keybindable command");
        }
    }
}
