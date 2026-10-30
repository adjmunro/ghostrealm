//! The web surface's process boundary. Chromium (CEF) runs in a separate host
//! process (`crates/web-host`) so a renderer crash or hang never takes the
//! multiplexer — and its shells — down with it, and so the app builds without
//! CEF. This crate is the shared contract: the line-delimited JSON [`protocol`],
//! the shared-memory [`frame`] transport, and the app-side [`client`].

pub mod client;
pub mod frame;
pub mod protocol;
pub mod temp;

pub use client::{find_host, Exiting, FrameInfo, PopupInfo, SharedFrame, ViewState, WebHost};
pub use protocol::{Cursor, EditAction, Event, Key, KeyKind, MouseAction, MouseButton, PopupKind, Request};
