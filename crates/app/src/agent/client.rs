//! `ghostrealm agent` in a window's terminal: relays the agent protocol
//! between stdin/stdout and the window's socket ([`super::socket`]), as the
//! workspace whose token the terminal holds.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};

/// A connection to the window's socket, past its `hello`: each request is
/// served as the workspace whose token opened it.
pub struct Connection {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Connection {
    /// Connect to `socket` and say `hello` with `token`. A socket that can't
    /// be reached or a refused `hello` is an error.
    pub fn open(socket: &Path, token: &str) -> Result<Self> {
        let stream = UnixStream::connect(socket)
            .with_context(|| format!("can't reach the window's agent socket {}", socket.display()))?;
        let reader = BufReader::new(stream.try_clone().context("cloning the agent socket")?);
        let mut conn = Self { stream, reader };
        let hello: Json = serde_json::from_str(&conn.request(&json!({ "op": "hello", "token": token }).to_string())?)?;
        if hello["ok"] != json!(true) {
            bail!("the window refused this terminal's token: {}", hello["error"].as_str().unwrap_or("no reason given"));
        }
        Ok(conn)
    }

    /// Send one request line (no newline) and return the reply line, newline
    /// included. A window that hangs up is an error.
    pub fn request(&mut self, line: &str) -> Result<String> {
        let mut buf = line.as_bytes().to_vec();
        buf.push(b'\n');
        (&self.stream)
            .write_all(&buf)
            .and_then(|()| (&self.stream).flush())
            .context("the window hung up the agent channel")?;
        let mut reply = String::new();
        if self.reader.read_line(&mut reply).context("reading the window's reply")? == 0 {
            bail!("the window hung up the agent channel");
        }
        Ok(reply)
    }
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
