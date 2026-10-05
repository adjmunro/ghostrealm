//! A minimal language server driven through the public client, both in-process
//! and over framed bytes (what a server process sees on stdio).

use std::io::{Read, Write};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use ghostrealm_lsp::types::{
    Diagnostic, DiagnosticSeverity, Hover, HoverContents, HoverProviderCapability, MarkupContent,
    MarkupKind, Position, ServerCapabilities, Uri,
};
use ghostrealm_lsp::{io_connection, serve, Client, Document, Encoding, Event, LanguageServer};

/// Flags every `TODO`; hover shows the word under the cursor.
struct Words;

impl LanguageServer for Words {
    fn name(&self) -> &str {
        "words"
    }

    fn capabilities(&self) -> ServerCapabilities {
        ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        }
    }

    fn diagnostics(&mut self, doc: &Document) -> Vec<Diagnostic> {
        doc.text
            .match_indices("TODO")
            .map(|(i, m)| Diagnostic {
                range: doc.range(i..i + m.len()),
                severity: Some(DiagnosticSeverity::WARNING),
                message: "unfinished".into(),
                ..Default::default()
            })
            .collect()
    }

    fn hover(&mut self, doc: &Document, pos: Position) -> Option<Hover> {
        let off = doc.offset(pos)?;
        let is_word = |c: char| c.is_alphanumeric();
        let start = doc.text[..off].rfind(|c| !is_word(c)).map_or(0, |i| i + 1);
        let end = doc.text[off..].find(|c| !is_word(c)).map_or(doc.text.len(), |i| off + i);
        let word = &doc.text[start..end];
        (!word.is_empty()).then(|| Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::PlainText,
                value: format!("word: {word}"),
            }),
            range: Some(doc.range(start..end)),
        })
    }
}

fn uri() -> Uri {
    "file:///tmp/notes.txt".parse().unwrap()
}

/// Poll until an event matches, or fail after a few seconds.
fn wait_for<T>(client: &mut Client, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        for e in client.poll() {
            if let Some(t) = pick(e) {
                return t;
            }
        }
        assert!(Instant::now() < deadline, "timed out waiting for the server");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn diagnostics(client: &mut Client) -> Vec<Diagnostic> {
    wait_for(client, |e| match e {
        Event::Diagnostics(d) => Some(d),
        _ => None,
    })
}

/// Open, diagnose, hover, edit, re-diagnose.
fn exercise(mut client: Client) {
    client.open(uri(), "plaintext", "a TODO\né😀 word".into());
    let d = diagnostics(&mut client);
    assert_eq!(d.len(), 1);
    assert_eq!(d[0].range.start, Position { line: 0, character: 2 });
    assert!(client.is_ready());
    // The client prefers UTF-32 (its editor counts characters).
    assert_eq!(client.encoding(), Encoding::Utf32);

    // "word" starts after 'é', '😀', ' ': character 3 in UTF-32.
    let id = client.hover(Position { line: 1, character: 4 }).unwrap();
    let hover = wait_for(&mut client, |e| match e {
        Event::Hover { request, hover } if request == id => Some(hover),
        _ => None,
    });
    let hover = hover.expect("a word is under the cursor");
    let HoverContents::Markup(m) = hover.contents else { panic!("plain text expected") };
    assert_eq!(m.value, "word: word");
    assert_eq!(hover.range.unwrap().start, Position { line: 1, character: 3 });

    client.change("done\n".into());
    assert_eq!(diagnostics(&mut client), vec![]);
}

#[test]
fn in_process() {
    exercise(Client::in_process(Words, None));
}

#[test]
fn over_framed_bytes() {
    let (to_server, server_in) = pipe();
    let (server_out, from_server) = pipe();
    std::thread::spawn(move || {
        let conn = io_connection(server_in, server_out, None);
        serve(&mut Words, &conn).expect("clean shutdown");
    });
    exercise(Client::over(io_connection(from_server, to_server, None)));
}

#[test]
fn a_server_that_dies_reports_stopped() {
    let (to_server, server_in) = pipe();
    let (server_out, from_server) = pipe();
    // Read the initialize request, then hang up without answering.
    std::thread::spawn(move || {
        let mut server_in = server_in;
        let _ = server_in.read(&mut [0u8; 64]);
        drop(server_out);
    });
    let mut client = Client::over(io_connection(from_server, to_server, None));
    wait_for(&mut client, |e| matches!(e, Event::Stopped).then_some(()));
    assert!(!client.is_ready());
}

/// An in-memory byte pipe: (write end, read end).
fn pipe() -> (PipeWriter, PipeReader) {
    let (tx, rx) = crossbeam_channel::unbounded();
    (PipeWriter(tx), PipeReader { rx, buf: Vec::new(), pos: 0 })
}

struct PipeWriter(Sender<Vec<u8>>);

impl Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.send(buf.to_vec()).map_err(std::io::Error::other)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct PipeReader {
    rx: Receiver<Vec<u8>>,
    buf: Vec<u8>,
    pos: usize,
}

impl Read for PipeReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.pos == self.buf.len() {
            match self.rx.recv() {
                Ok(b) => (self.buf, self.pos) = (b, 0),
                Err(_) => return Ok(0),
            }
        }
        let n = out.len().min(self.buf.len() - self.pos);
        out[..n].copy_from_slice(&self.buf[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}
