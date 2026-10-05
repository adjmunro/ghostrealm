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
        if self.config_path.as_deref() == Some(path) {
            let server = ConfigServer::new(self.catalogue.clone());
            return Some((Client::in_process(server, waker), "toml"));
        }
        None
    }
}
