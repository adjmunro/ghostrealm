//! `ghostrealm agent` in a window's terminal: relays the agent protocol
//! between stdin/stdout and the window's socket ([`super::socket`]), as the
//! workspace whose token the terminal holds.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value as Json};

use super::socket::MAX_LINE;

/// A connection to the window's socket, past its `hello`: each request is
/// served as the workspace whose token opened it. One the window closed
/// for being idle is opened again.
pub struct Connection {
    socket: PathBuf,
    token: String,
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    /// Connect to `socket` and say `hello` with `token`. A socket that can't
    /// be reached or a refused `hello` is an error.
    pub fn open(socket: &Path, token: &str) -> Result<Self> {
        let (stream, reader) = connect(socket, token)?;
        Ok(Self { socket: socket.to_path_buf(), token: token.to_string(), stream, reader })
    }

    /// Send one request line (no newline) and return the reply line, newline
    /// included. A line longer than the window takes is answered here, with
    /// an error reply, and not sent. If the window closed the connection for
    /// being idle, which it does only when it read no request since its last
    /// reply, it is opened again and the line sent once more. A window that
    /// hangs up otherwise, or refuses the reopening, is an error.
    pub fn request(&mut self, line: &str) -> Result<String> {
        if line.len() + 1 > MAX_LINE {
            let error = format!("request too long: {} bytes; the window takes at most {MAX_LINE}", line.len() + 1);
            return Ok(format!("{}\n", json!({ "ok": false, "error": error })));
        }
        let reply = exchange(&self.stream, &mut self.reader, line)?;
        if !closed_idle(&reply) {
            return Ok(reply);
        }
        (self.stream, self.reader) = connect(&self.socket, &self.token)?;
        let reply = exchange(&self.stream, &mut self.reader, line)?;
        if closed_idle(&reply) {
            bail!("the window closed the agent channel unread twice");
        }
        Ok(reply)
    }
}

/// Connect to `socket` and say `hello` with `token`.
fn connect(socket: &Path, token: &str) -> Result<(UnixStream, BufReader<UnixStream>)> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("can't reach the window's agent socket {}", socket.display()))?;
    let mut reader = BufReader::new(stream.try_clone().context("cloning the agent socket")?);
    let hello: Json = serde_json::from_str(&exchange(&stream, &mut reader, &json!({ "op": "hello", "token": token }).to_string())?)?;
    if hello["ok"] != json!(true) {
        bail!("the window refused the connection: {}", hello["error"].as_str().unwrap_or("no reason given"));
    }
    Ok((stream, reader))
}

/// Write `line` and read the reply line. If the write fails, the one line
/// the window may have sent before closing (its idle line) is read and
/// returned; otherwise the write's error is.
fn exchange(stream: &UnixStream, reader: &mut BufReader<UnixStream>, line: &str) -> Result<String> {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    let wrote = (&*stream).write_all(&buf).and_then(|()| (&*stream).flush());
    let mut reply = String::new();
    let read = reader.read_line(&mut reply);
    match (wrote, read) {
        (Ok(()), Ok(n)) if n > 0 => Ok(reply),
        (Err(_), Ok(n)) if n > 0 && closed_idle(&reply) => Ok(reply),
        (Err(e), _) => Err(anyhow!(e).context("the window hung up the agent channel")),
        (Ok(()), Ok(_)) => bail!("the window hung up the agent channel"),
        (Ok(()), Err(e)) => Err(anyhow!(e).context("reading the window's reply")),
    }
}

/// Whether `reply` is the window's line closing an idle connection, sent only
/// when no request was read since the last reply.
fn closed_idle(reply: &str) -> bool {
    serde_json::from_str::<Json>(reply).is_ok_and(|r| r.get("ok") == Some(&json!(false)) && r.get("closed") == Some(&json!("idle")))
}

/// Say `hello` with `token` at `socket`, then relay each non-empty request
/// line from `input` and write its reply to `output`, until `input` ends.
/// A socket that can't be reached, a refused `hello`, or a window that hangs
/// up is an error.
pub fn relay(socket: &Path, token: &str, input: impl BufRead, mut output: impl Write) -> Result<()> {
    let mut conn = Connection::open(socket, token)?;
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        output.write_all(conn.request(line)?.as_bytes())?;
        output.flush()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::testing::TmpDir;
    use std::os::unix::net::UnixListener;

    const IDLE: &str = r#"{"ok":false,"error":"closed after 1s without a request","closed":"idle"}"#;

    /// A fake window's connection: reads `hello` and accepts it, then either
    /// writes the idle line and closes unread, or answers one request with
    /// `{"ok":true,"n":<n>}` and returns that request.
    fn serve_one(listener: &UnixListener, n: u32, idle: bool) -> Option<String> {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        writeln!(&stream, r#"{{"ok":true,"workspace":1}}"#).unwrap();
        if idle {
            writeln!(&stream, "{IDLE}").unwrap();
            return None;
        }
        line.clear();
        reader.read_line(&mut line).unwrap();
        writeln!(&stream, r#"{{"ok":true,"n":{n}}}"#).unwrap();
        Some(line)
    }

    /// Given a window that closes the connection as idle without reading the
    /// next request;
    /// when a request is sent;
    /// then it is sent again on a new connection, and that one's reply is
    /// returned.
    #[test]
    fn a_request_the_window_closed_unread_is_sent_again_once() {
        let dir = TmpDir::new("cl-idle");
        let path = dir.join("w.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let window = std::thread::spawn(move || {
            assert_eq!(serve_one(&listener, 1, true), None);
            serve_one(&listener, 2, false)
        });
        let mut conn = Connection::open(&path, "t").unwrap();
        assert_eq!(conn.request(r#"{"op":"pump"}"#).unwrap(), "{\"ok\":true,\"n\":2}\n");
        assert_eq!(window.join().unwrap().as_deref(), Some("{\"op\":\"pump\"}\n"));
    }

    /// Given a window that closes each connection as idle without reading
    /// the next request;
    /// when a request is sent;
    /// then it is an error after the second connection, and no third is
    /// opened.
    #[test]
    fn a_request_is_sent_again_at_most_once() {
        let dir = TmpDir::new("cl-twice");
        let path = dir.join("w.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let window = std::thread::spawn(move || {
            serve_one(&listener, 1, true);
            serve_one(&listener, 2, true);
            listener
        });
        let mut conn = Connection::open(&path, "t").unwrap();
        let error = format!("{:#}", conn.request(r#"{"op":"pump"}"#).unwrap_err());
        assert!(error.contains("unread twice"), "{error}");
        let listener = window.join().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(listener.accept().unwrap_err().kind(), std::io::ErrorKind::WouldBlock, "no third connection");
    }

    /// Given the idle line, and replies that carry its words without being
    /// it (`ok` true, the text inside a string, a cut-off line);
    /// when each is checked;
    /// then only the idle line counts.
    #[test]
    fn only_the_idle_line_itself_reopens() {
        assert!(closed_idle(IDLE));
        assert!(!closed_idle(r#"{"ok":true,"closed":"idle"}"#));
        assert!(!closed_idle(r#"{"ok":false,"error":"\"closed\":\"idle\""}"#));
        assert!(!closed_idle(r#"{"ok":false,"closed":"idle""#));
    }
}
