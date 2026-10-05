//! Language services for the editor, over the Language Server Protocol.
//!
//! Every language service talks to the editor in standard LSP messages, so a
//! first-party server and an existing external one (rust-analyzer, a Kotlin
//! server, ...) plug in the same way:
//!
//! - A first-party server implements [`LanguageServer`]: a name, its
//!   capabilities, and a hook per feature. [`serve`] runs it over any
//!   [`Connection`]: in-process on a thread, or over stdio as a standalone
//!   server other editors can use too.
//! - The editor holds one [`Client`] per open document, created with
//!   [`Client::in_process`] or [`Client::spawn`] (an external server process).
//!   Dropping it closes the document and shuts the server down, so a server
//!   runs only while its document is open.

pub mod client;
pub mod server;
pub mod text;

pub use client::{io_connection, Client, Event, Waker};
pub use lsp_server::{Connection, RequestId};
pub use lsp_types as types;
pub use server::{serve, Document, LanguageServer, ServeError};
pub use text::Encoding;
