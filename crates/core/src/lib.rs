//! Platform-agnostic app core: the command registry (the spine shared by the
//! palette, keybindings, and the agent channel) and — as they land — the
//! tab/split tree, inbox state machine, config, and keybinding resolution.
//!
//! Built and tested headless; depends on no windowing, GPU, VT, or PTY code.

pub mod command;
pub mod fuzzy;
pub mod registry;

pub use command::{ArgError, ArgKind, ArgSpec, Args, CommandMeta, Value};
pub use registry::{CmdError, CmdOutcome, Registry, SearchHit};
