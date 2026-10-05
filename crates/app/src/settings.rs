//! The config file as the settings UI: the option catalogue it is checked and
//! documented against (the app's sections plus each plugin's).

use ghostrealm_core::config::{core_sections, Catalogue, KeyDoc};
use ghostrealm_core::Registry;

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
        let mut cfg = Config::default();
        cfg.chrome = Some(Default::default());
        cfg.workspace.default_directory = Some("~".into());
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
    fn every_default_binding_is_a_listed_command() {
        let reg = build_registry();
        let listed: Vec<&str> = super::keybindable_commands(&reg).iter().map(|k| k.key).collect();
        for id in ghostrealm_core::config::default_binding_ids() {
            assert!(listed.contains(&id), "default binding for `{id}` isn't a keybindable command");
        }
    }
}
