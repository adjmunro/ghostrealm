//! The text editor plugin: a minimal editable buffer with line numbers,
//! soft-wrap, selection, and TOML highlighting.

pub mod buffer;
pub mod toml;
pub mod wrap;

pub use buffer::{EditorBuffer, Motion};

/// Editor text colour.
pub const EDITOR_FG: [u8; 3] = [220, 220, 230];
