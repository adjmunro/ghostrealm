//! Platform-agnostic app core: command registry, tab/split tree, inbox state
//! machine, config, keybinding resolution. Built and tested headless.
//!
//! Populated in Phase 2; this crate exists now to fix the workspace boundary
//! (the app depends on the core, never the reverse).
