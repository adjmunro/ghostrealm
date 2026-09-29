//! The first-party plugins, built on the same [`crate::plugin`] API any other
//! plugin uses.

pub mod file_browser;

use crate::plugin::Plugin;

/// Every built-in plugin, in "nothing open" picker order. When a file is opened,
/// the first plugin that claims its path wins, so catch-alls go last.
pub fn builtin() -> Vec<Box<dyn Plugin>> {
    vec![Box::new(file_browser::FileBrowserPlugin)]
}
