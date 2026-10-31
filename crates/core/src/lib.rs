//! Platform-agnostic app core: the command registry (the spine shared by the
//! palette, keybindings, and the agent channel) and — as they land — the
//! tab/split tree, inbox state machine, config, and keybinding resolution.
//!
//! Built and tested headless; depends on no windowing, GPU, VT, or PTY code.

pub mod command;
pub mod config;
pub mod fs_tree;
pub mod fuzzy;
pub mod ghostty;
pub mod git;
pub mod highlight;
pub mod line_diff;
pub mod outline;
pub mod registry;
pub mod search;
pub mod sidebar;
pub mod state;
pub mod tab_drop;
#[cfg(test)]
mod test_dir;
pub mod tree;

pub use config::{Chrome, CloseOnExit, Config, HeaderIcon, Headings, Inbox, LineNumbers, Side, Web};

pub use command::{ArgError, ArgKind, ArgSpec, Args, CommandMeta, Value};
pub use registry::{CmdError, CmdOutcome, Registry, SearchHit};
pub use tab_drop::{Edge, TabDrop};
pub use tree::{Axis, Divider, Node, Pane, PaneId, Rect, SplitPath, Surface, SurfaceId, TabStatus, Tree, Vtab, VtabId};
