//! The editor's side of a language service: one [`Client`] per open document.
//! It speaks LSP to either a first-party server running in-process on its own
//! thread or an external server process, so the editor never knows which.
//!
//! Calls never block: requests and notifications are queued until the
//! `initialize` handshake completes, and replies arrive as [`Event`]s from
//! [`Client::poll`] (the waker fires when one is ready).

use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use lsp_server::{Connection, ErrorCode, Message, Notification, Request, RequestId, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Exit, Initialized,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{
    HoverRequest, Initialize, RegisterCapability, Request as _, Shutdown, WorkDoneProgressCreate,
    WorkspaceConfiguration,
};
use lsp_types::{
    ClientCapabilities, ClientInfo, ConfigurationParams, Diagnostic, DidChangeTextDocumentParams,
    DidCloseTextDocumentParams, DidOpenTextDocumentParams, GeneralClientCapabilities, Hover,
    HoverClientCapabilities, HoverParams, InitializeParams, InitializeResult, MarkupKind, Position,
    PublishDiagnosticsClientCapabilities, PublishDiagnosticsParams,
    TextDocumentClientCapabilities, TextDocumentContentChangeEvent, TextDocumentIdentifier,
    TextDocumentItem, TextDocumentPositionParams, Uri, VersionedTextDocumentIdentifier,
    WorkDoneProgressParams, WorkspaceFolder,
};

use crate::server::{serve, LanguageServer};
use crate::text::Encoding;

/// Called from any thread when an [`Event`] may be ready.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

/// How long a server process gets to exit after `shutdown` before it is killed.
const EXIT_GRACE: Duration = Duration::from_secs(2);

/// Something the server sent.
#[derive(Debug)]
pub enum Event {
    /// The full set of diagnostics for the open document.
    Diagnostics(Vec<Diagnostic>),
    /// The answer to [`Client::hover`].
    Hover { request: RequestId, hover: Option<Hover> },
    /// The server is gone (it exited, crashed, or failed the handshake).
    Stopped,
}

enum State {
    /// Waiting for the `initialize` reply; everything else is queued.
    Initializing { id: RequestId, queued: Vec<Message> },
    Ready,
    Stopped,
}

/// One document's connection to a language server.
pub struct Client {
    tx: Sender<Message>,
    rx: Receiver<Message>,
    state: State,
    next_id: i32,
    encoding: Encoding,
    doc: Option<(Uri, i32)>,
    /// Outstanding requests by id, with their method.
    pending: HashMap<RequestId, &'static str>,
    child: Option<Child>,
}

impl Client {
    /// Run a first-party `server` on its own thread, talking LSP over channels.
    pub fn in_process(mut server: impl LanguageServer + 'static, waker: Option<Waker>) -> Client {
        let (ours, theirs) = Connection::memory();
        std::thread::Builder::new()
            .name(format!("lsp {}", server.name()))
            .spawn(move || {
                let _ = serve(&mut server, &theirs);
            })
            .expect("spawn language server thread");
        let rx = forward(ours.receiver, waker);
        Client::start(ours.sender, rx, None, None)
    }

    /// Start an external server process speaking LSP on its stdin/stdout.
    /// `root` is the workspace folder it should treat as the project.
    pub fn spawn(mut command: Command, root: Option<&Path>, waker: Option<Waker>) -> io::Result<Client> {
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        let (stdin, stdout) = (child.stdin.take(), child.stdout.take());
        let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
            return Err(io::Error::other("server stdio unavailable"));
        };
        let conn = io_connection(stdout, stdin, waker);
        Ok(Client::start(conn.sender, conn.receiver, root, Some(child)))
    }

    /// A client over an established connection (e.g. [`io_connection`]).
    pub fn over(conn: Connection) -> Client {
        Client::start(conn.sender, conn.receiver, None, None)
    }

    fn start(tx: Sender<Message>, rx: Receiver<Message>, root: Option<&Path>, child: Option<Child>) -> Client {
        let id = RequestId::from(0);
        #[allow(deprecated)] // `root_uri`: some servers still read only this.
        let params = InitializeParams {
            process_id: Some(std::process::id()),
            root_uri: root.map(crate::text::file_uri),
            workspace_folders: root.map(|r| {
                vec![WorkspaceFolder {
                    uri: crate::text::file_uri(r),
                    name: r.file_name().map_or_else(String::new, |n| n.to_string_lossy().into()),
                }]
            }),
            capabilities: capabilities(),
            client_info: Some(ClientInfo { name: "ghostrealm".into(), version: None }),
            ..Default::default()
        };
        let _ = tx.send(Request::new(id.clone(), Initialize::METHOD.into(), params).into());
        Client {
            tx,
            rx,
            state: State::Initializing { id, queued: Vec::new() },
            next_id: 1,
            encoding: Encoding::default(),
            doc: None,
            pending: HashMap::new(),
            child,
        }
    }

    /// The unit LSP columns count in (known once the handshake completes;
    /// UTF-16 until then, and nothing position-bearing is sent before).
    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Open the document. One per client.
    pub fn open(&mut self, uri: Uri, language_id: &str, text: String) {
        let params = DidOpenTextDocumentParams {
            text_document: TextDocumentItem {
                uri: uri.clone(),
                language_id: language_id.into(),
                version: 0,
                text,
            },
        };
        self.doc = Some((uri, 0));
        self.notify(DidOpenTextDocument::METHOD, params);
    }

    /// The document's text changed: send all of it.
    pub fn change(&mut self, text: String) {
        let Some((uri, version)) = self.doc.as_mut() else {
            return;
        };
        *version += 1;
        let params = DidChangeTextDocumentParams {
            text_document: VersionedTextDocumentIdentifier { uri: uri.clone(), version: *version },
            content_changes: vec![TextDocumentContentChangeEvent { range: None, range_length: None, text }],
        };
        self.notify(DidChangeTextDocument::METHOD, params);
    }

    /// Ask for hover docs at `pos` (in [`encoding`](Self::encoding) units, so
    /// only meaningful once the handshake completed). The answer arrives as
    /// [`Event::Hover`] with the returned id.
    pub fn hover(&mut self, pos: Position) -> Option<RequestId> {
        let uri = self.doc.as_ref()?.0.clone();
        let params = HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        };
        Some(self.request(HoverRequest::METHOD, params))
    }

    /// Whether the handshake completed (positions can be sent).
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::Ready)
    }

    /// Drain what the server sent.
    pub fn poll(&mut self) -> Vec<Event> {
        let mut events = Vec::new();
        loop {
            match self.rx.try_recv() {
                Ok(msg) => self.handle(msg, &mut events),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if !matches!(self.state, State::Stopped) {
                        self.state = State::Stopped;
                        events.push(Event::Stopped);
                    }
                    break;
                }
            }
        }
        events
    }

    fn handle(&mut self, msg: Message, events: &mut Vec<Event>) {
        match msg {
            Message::Response(resp) => {
                if let State::Initializing { id, queued } = &mut self.state {
                    if *id != resp.id {
                        return;
                    }
                    let queued = std::mem::take(queued);
                    let init = resp.response_result.ok().and_then(|v| serde_json::from_value::<InitializeResult>(v).ok());
                    let Some(init) = init else {
                        self.state = State::Stopped;
                        events.push(Event::Stopped);
                        return;
                    };
                    self.encoding = init
                        .capabilities
                        .position_encoding
                        .as_ref()
                        .and_then(Encoding::from_kind)
                        .unwrap_or_default();
                    self.state = State::Ready;
                    self.send(Notification::new(Initialized::METHOD.into(), serde_json::json!({})).into());
                    for m in queued {
                        self.send(m);
                    }
                    return;
                }
                let Some(method) = self.pending.remove(&resp.id) else {
                    return;
                };
                if method == HoverRequest::METHOD {
                    let hover = resp.response_result.ok().and_then(|v| serde_json::from_value(v).ok());
                    events.push(Event::Hover { request: resp.id, hover });
                }
            }
            Message::Notification(n) => {
                if n.method == PublishDiagnostics::METHOD {
                    let Ok(p) = serde_json::from_value::<PublishDiagnosticsParams>(n.params) else {
                        return;
                    };
                    if self.doc.as_ref().is_some_and(|(uri, _)| *uri == p.uri) {
                        events.push(Event::Diagnostics(p.diagnostics));
                    }
                }
            }
            // Servers ask clients a few things; answer so they don't wait.
            Message::Request(req) => {
                let resp = match req.method.as_str() {
                    RegisterCapability::METHOD | WorkDoneProgressCreate::METHOD => {
                        Response::new_ok(req.id, ())
                    }
                    WorkspaceConfiguration::METHOD => {
                        let n = serde_json::from_value::<ConfigurationParams>(req.params)
                            .map_or(0, |p| p.items.len());
                        Response::new_ok(req.id, vec![serde_json::Value::Null; n])
                    }
                    _ => Response::new_err(
                        req.id,
                        ErrorCode::MethodNotFound as i32,
                        format!("unsupported request {}", req.method),
                    ),
                };
                self.send(resp.into());
            }
        }
    }

    fn request(&mut self, method: &'static str, params: impl serde::Serialize) -> RequestId {
        let id = RequestId::from(self.next_id);
        self.next_id += 1;
        self.pending.insert(id.clone(), method);
        self.queue(Request::new(id.clone(), method.into(), params).into());
        id
    }

    fn notify(&mut self, method: &str, params: impl serde::Serialize) {
        self.queue(Notification::new(method.into(), params).into());
    }

    fn queue(&mut self, msg: Message) {
        match &mut self.state {
            State::Initializing { queued, .. } => queued.push(msg),
            State::Ready => self.send(msg),
            State::Stopped => {}
        }
    }

    fn send(&mut self, msg: Message) {
        if self.tx.send(msg).is_err() {
            self.state = State::Stopped;
        }
    }
}

impl Drop for Client {
    /// Close the document and shut the server down. A server process that
    /// doesn't exit within [`EXIT_GRACE`] is killed (off the UI thread).
    fn drop(&mut self) {
        if self.is_ready() {
            if let Some((uri, _)) = self.doc.take() {
                let params = DidCloseTextDocumentParams { text_document: TextDocumentIdentifier { uri } };
                self.notify(DidCloseTextDocument::METHOD, params);
            }
            let id = RequestId::from(self.next_id);
            self.send(Request::new(id, Shutdown::METHOD.into(), ()).into());
            self.send(Notification::new(Exit::METHOD.into(), ()).into());
        }
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                let deadline = Instant::now() + EXIT_GRACE;
                while Instant::now() < deadline {
                    if let Ok(Some(_)) = child.try_wait() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                let _ = child.kill();
                let _ = child.wait();
            });
        }
    }
}

/// What this client supports, announced in `initialize`.
fn capabilities() -> ClientCapabilities {
    ClientCapabilities {
        general: Some(GeneralClientCapabilities {
            // The editor indexes characters, so UTF-32 needs no conversion.
            position_encodings: Some(vec![Encoding::Utf32.kind(), Encoding::Utf8.kind(), Encoding::Utf16.kind()]),
            ..Default::default()
        }),
        text_document: Some(TextDocumentClientCapabilities {
            hover: Some(HoverClientCapabilities {
                dynamic_registration: Some(false),
                content_format: Some(vec![MarkupKind::PlainText]),
            }),
            publish_diagnostics: Some(PublishDiagnosticsClientCapabilities {
                version_support: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Relay `rx` to a new channel, waking `waker` after each message.
fn forward(rx: Receiver<Message>, waker: Option<Waker>) -> Receiver<Message> {
    let (tx, out) = crossbeam_channel::unbounded();
    std::thread::spawn(move || {
        for msg in rx {
            if tx.send(msg).is_err() {
                break;
            }
            if let Some(w) = &waker {
                w();
            }
        }
        // Wake once more so the hang-up is noticed.
        drop(tx);
        if let Some(w) = &waker {
            w();
        }
    });
    out
}

/// A connection speaking LSP's framed JSON-RPC over a byte stream pair (a
/// child's stdout/stdin, a socket, a pipe). `waker` fires after each message
/// read.
pub fn io_connection(
    read: impl Read + Send + 'static,
    mut write: impl Write + Send + 'static,
    waker: Option<Waker>,
) -> Connection {
    let (out_tx, out_rx) = crossbeam_channel::unbounded::<Message>();
    let (in_tx, in_rx) = crossbeam_channel::unbounded::<Message>();
    std::thread::spawn(move || {
        for msg in out_rx {
            if msg.write(&mut write).is_err() {
                break;
            }
        }
    });
    std::thread::spawn(move || {
        let mut read = BufReader::new(read);
        while let Ok(Some(msg)) = Message::read(&mut read) {
            if in_tx.send(msg).is_err() {
                break;
            }
            if let Some(w) = &waker {
                w();
            }
        }
        drop(in_tx);
        if let Some(w) = &waker {
            w();
        }
    });
    Connection { sender: out_tx, receiver: in_rx }
}
