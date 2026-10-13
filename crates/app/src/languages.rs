//! Which language service an editor gets for a file. Each service runs only
//! while its file is open: the editor owns the [`Client`], and dropping it
//! shuts the server down.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ghostrealm_core::config::Catalogue;
use ghostrealm_lsp::{Client, Waker};

use crate::settings::ConfigServer;

pub struct Languages {
    config_path: Option<PathBuf>,
    catalogue: Arc<Catalogue>,
}

impl Languages {
    /// Services for the config file at `config_path` (checked against
    /// `catalogue`).
    pub fn new(config_path: Option<PathBuf>, catalogue: Arc<Catalogue>) -> Self {
        Languages { config_path, catalogue }
    }

    /// Start the service for `path`, with its LSP language id, if it has one.
    pub fn start(&self, path: &Path, waker: Option<Waker>) -> Option<(Client, &'static str)> {
        if self.config_path.as_deref().is_some_and(|c| same_file(c, path)) {
            let server = ConfigServer::new(self.catalogue.clone());
            return Some((Client::in_process(server, waker), "toml"));
        }
        None
    }
}

/// Whether two paths name the same file: equal, or equal once symlinks and
/// `..` are resolved.
pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    a == b
        || match (a.canonicalize(), b.canonicalize()) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn the_config_file_gets_its_service_through_a_symlink() {
        let dir = crate::plugin::testing::tmpdir("languages-symlink");
        let real = dir.join("config.toml");
        std::fs::write(&real, "").unwrap();
        let link = dir.join("link.toml");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let other = dir.join("other.toml");
        std::fs::write(&other, "").unwrap();
        let catalogue = Arc::new(crate::settings::catalogue(&crate::app_state::build_registry()));
        let languages = Languages::new(Some(real), catalogue);
        assert!(languages.start(&link, None).is_some());
        assert!(languages.start(&other, None).is_none());
    }
}
