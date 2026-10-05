//! Hosting a first-party language server. A server implements the few typed
//! hooks of [`LanguageServer`]; [`serve`] does the protocol around them: the
//! `initialize` handshake and position-encoding negotiation, document sync,
//! publishing diagnostics, request dispatch, and `shutdown`/`exit`.

use std::collections::HashMap;
use std::ops::Range;

use lsp_server::{Connection, ErrorCode, Message, Notification, Request, Response};
use lsp_types::notification::{
    DidChangeTextDocument, DidCloseTextDocument, DidOpenTextDocument, Exit,
    Notification as _, PublishDiagnostics,
};
use lsp_types::request::{HoverRequest, Request as _};
use lsp_types::{
    Diagnostic, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Hover, HoverParams, InitializeParams, Position,
    PublishDiagnosticsParams, ServerCapabilities, ServerInfo, TextDocumentSyncCapability,
    TextDocumentSyncKind, Uri,
};

use crate::text::{self, Encoding};

/// An open document, as a server sees it.
pub struct Document {
    pub uri: Uri,
    pub language_id: String,
    pub version: i32,
    pub text: String,
    encoding: Encoding,
}

impl Document {
    /// The byte offset of an LSP position.
    pub fn offset(&self, pos: Position) -> Option<usize> {
        text::offset(&self.text, pos, self.encoding)
    }

    /// The LSP range of a byte span.
    pub fn range(&self, span: Range<usize>) -> lsp_types::Range {
        lsp_types::Range {
            start: text::position(&self.text, span.start, self.encoding),
            end: text::position(&self.text, span.end, self.encoding),
        }
    }
}

/// A first-party language server. Every hook but [`name`](Self::name) and
/// [`capabilities`](Self::capabilities) defaults to "nothing to say"; declare
/// each feature you implement in `capabilities`.
pub trait LanguageServer: Send {
    /// Reported as `serverInfo.name`.
    fn name(&self) -> &str;

    /// The features this server provides. [`serve`] fills in text sync and the
    /// position encoding.
    fn capabilities(&self) -> ServerCapabilities;

    /// Diagnostics for `doc`, published after it opens and after each change.
    fn diagnostics(&mut self, _doc: &Document) -> Vec<Diagnostic> {
        Vec::new()
    }

    /// `textDocument/hover` at `pos`.
    fn hover(&mut self, _doc: &Document, _pos: Position) -> Option<Hover> {
        None
    }
}

/// Why [`serve`] stopped early.
pub type ServeError = Box<dyn std::error::Error + Send + Sync>;

/// Run `server` over `conn` until the client shuts it down, exits, or hangs up.
pub fn serve(server: &mut dyn LanguageServer, conn: &Connection) -> Result<(), ServeError> {
    let (id, params) = conn.initialize_start()?;
    let params: InitializeParams = serde_json::from_value(params)?;
    // The client lists encodings by preference; UTF-16 is the protocol's
    // fallback when it lists none we know.
    let encoding = params
        .capabilities
        .general
        .and_then(|g| g.position_encodings)
        .unwrap_or_default()
        .iter()
        .find_map(Encoding::from_kind)
        .unwrap_or_default();
    let mut caps = server.capabilities();
    caps.position_encoding = Some(encoding.kind());
    caps.text_document_sync = Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL));
    let result = serde_json::json!({
        "capabilities": caps,
        "serverInfo": ServerInfo { name: server.name().to_string(), version: None },
    });
    conn.initialize_finish(id, result)?;

    // Keyed by the URI's text: `Uri` caches parsed parts internally.
    let mut docs: HashMap<String, Document> = HashMap::new();
    for msg in &conn.receiver {
        match msg {
            Message::Request(req) => {
                if conn.handle_shutdown(&req)? {
                    return Ok(());
                }
                let resp = respond(server, &docs, req);
                send(conn, resp.into())?;
            }
            Message::Notification(n) => {
                if n.method == Exit::METHOD {
                    return Ok(());
                }
                if let Some(uri) = sync(&mut docs, n, encoding) {
                    let doc = docs.get(uri.as_str());
                    let diagnostics = doc.map(|d| server.diagnostics(d)).unwrap_or_default();
                    let version = doc.map(|d| d.version);
                    let params = PublishDiagnosticsParams { uri, diagnostics, version };
                    send(conn, Notification::new(PublishDiagnostics::METHOD.into(), params).into())?;
                }
            }
            Message::Response(_) => {}
        }
    }
    Ok(())
}

fn send(conn: &Connection, msg: Message) -> Result<(), ServeError> {
    conn.sender.send(msg).map_err(|_| "the client hung up".into())
}

fn respond(server: &mut dyn LanguageServer, docs: &HashMap<String, Document>, req: Request) -> Response {
    match req.method.as_str() {
        HoverRequest::METHOD => match serde_json::from_value::<HoverParams>(req.params) {
            Ok(p) => {
                let pos = p.text_document_position_params;
                let hover = docs
                    .get(pos.text_document.uri.as_str())
                    .and_then(|d| server.hover(d, pos.position));
                Response::new_ok(req.id, hover)
            }
            Err(e) => Response::new_err(req.id, ErrorCode::InvalidParams as i32, e.to_string()),
        },
        _ => Response::new_err(
            req.id,
            ErrorCode::MethodNotFound as i32,
            format!("unsupported request {}", req.method),
        ),
    }
}

/// Apply a document-sync notification. Returns the document whose diagnostics
/// changed (a closed document's become empty).
fn sync(docs: &mut HashMap<String, Document>, n: Notification, encoding: Encoding) -> Option<Uri> {
    match n.method.as_str() {
        DidOpenTextDocument::METHOD => {
            let p: DidOpenTextDocumentParams = serde_json::from_value(n.params).ok()?;
            let d = p.text_document;
            let doc = Document {
                uri: d.uri.clone(),
                language_id: d.language_id,
                version: d.version,
                text: d.text,
                encoding,
            };
            docs.insert(d.uri.as_str().to_string(), doc);
            Some(d.uri)
        }
        DidChangeTextDocument::METHOD => {
            let p: DidChangeTextDocumentParams = serde_json::from_value(n.params).ok()?;
            let doc = docs.get_mut(p.text_document.uri.as_str())?;
            for change in p.content_changes {
                match change.range {
                    Some(r) => {
                        let start = doc.offset(r.start)?;
                        let end = doc.offset(r.end)?.max(start);
                        doc.text.replace_range(start..end, &change.text);
                    }
                    None => doc.text = change.text,
                }
            }
            doc.version = p.text_document.version;
            Some(p.text_document.uri)
        }
        DidCloseTextDocument::METHOD => {
            let p: DidCloseTextDocumentParams = serde_json::from_value(n.params).ok()?;
            docs.remove(p.text_document.uri.as_str());
            Some(p.text_document.uri)
        }
        _ => None,
    }
}
