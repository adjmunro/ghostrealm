//! The Files surface: a file sidebar (a [`FileBrowserView`]) beside the
//! document it opened, with a slim row above them holding the sidebar
//! button. Every file the app opens lands in one (see
//! `AppState::open_file`). The documents are views of their own: an
//! [`EditorView`](crate::plugins::editor::EditorView) for text, a
//! [`MediaView`] for images and audio, each under a [`header::DocHeader`].
//!
//! [`FileBrowserView`]: crate::plugins::file_browser::FileBrowserView

pub mod header;
pub(crate) mod media;
mod view;

use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use anyhow::Result;
use ghostrealm_core::{CmdError, CmdOutcome, CommandMeta, Registry};

use crate::app_state::AppState;
use crate::plugin::{OpenCx, Plugin, View};
pub use media::MediaView;
pub use view::FilesView;

pub const ID: &str = "files";

/// What the Files surfaces of this run share: the sidebar's pin and width
/// as last set, which a new surface starts with. Each surface then keeps
/// its own.
#[derive(Clone, Default)]
pub struct Session(Rc<Cell<Shared>>);

#[derive(Clone, Copy, Default)]
struct Shared {
    /// The sidebar button's last state; `None` until it is first clicked.
    pinned: Option<bool>,
    /// The sidebar's last dragged width (logical px).
    width: Option<f32>,
}

impl Session {
    fn pinned(&self) -> Option<bool> {
        self.0.get().pinned
    }

    fn set_pinned(&self, on: bool) {
        self.0.set(Shared { pinned: Some(on), ..self.0.get() });
    }

    fn width(&self) -> Option<f32> {
        self.0.get().width
    }

    fn set_width(&self, w: f32) {
        self.0.set(Shared { width: Some(w), ..self.0.get() });
    }
}

pub struct FilesPlugin;

impl Plugin for FilesPlugin {
    fn id(&self) -> &'static str {
        ID
    }

    fn title(&self) -> &'static str {
        "Files"
    }

    /// A Files surface showing only its sidebar, at the start directory.
    fn open(&self, cx: &OpenCx) -> Result<Box<dyn View>> {
        Ok(Box::new(FilesView::new(start_dir(cx), cx)))
    }

    /// Images and audio; the editor claims the rest.
    fn opens_path(&self, path: &Path) -> bool {
        media::is_media(path)
    }

    /// The document of an image or a track.
    fn open_path(&self, path: &Path, _cx: &OpenCx) -> Result<Box<dyn View>> {
        Ok(Box::new(MediaView::open(path.to_path_buf())))
    }

    fn register_commands(&self, r: &mut Registry<AppState>) {
        r.register(
            CommandMeta::new(
                "files.focus_sidebar",
                "Focus File Sidebar",
                "Move the keyboard to the focused Files surface's sidebar, showing it if hidden",
            ),
            Box::new(|s: &mut AppState, _| {
                let files = s
                    .focused_surface()
                    .and_then(|sid| s.view_mut(sid))
                    .and_then(|v| v.downcast_mut::<FilesView>())
                    .ok_or_else(|| CmdError::Failed("no Files surface is focused".into()))?;
                files.focus_sidebar();
                Ok(CmdOutcome::ok())
            }),
        );
    }
}

/// The directory a Files surface's sidebar starts in: the workspace's,
/// else the current one.
pub fn start_dir(cx: &OpenCx) -> PathBuf {
    cx.cwd
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| PathBuf::from("."))
}
