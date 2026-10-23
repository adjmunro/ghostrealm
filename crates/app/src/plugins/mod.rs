//! The first-party plugins, built on the same [`crate::plugin`] API any other
//! plugin uses.

pub mod editor;
pub mod file_browser;
pub mod files;
pub mod terminal;
pub mod web;

use crate::plugin::Plugin;

/// Every built-in plugin, in "nothing open" picker order. When a file is opened,
/// the first plugin that claims its path wins, so catch-alls go last.
pub fn builtin() -> Vec<Box<dyn Plugin>> {
    builtin_with(web::WebPlugin::new())
}

/// [`builtin`], with `web` as the web plugin.
pub fn builtin_with(web: web::WebPlugin) -> Vec<Box<dyn Plugin>> {
    vec![
        Box::new(terminal::TerminalPlugin),
        Box::new(files::FilesPlugin),
        Box::new(file_browser::FileBrowserPlugin),
        Box::new(editor::EditorPlugin),
        Box::new(web),
    ]
}
