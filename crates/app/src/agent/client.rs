//! `ghostrealm agent` in a window's terminal: relays the agent protocol
//! between stdin/stdout and the window's socket ([`super::socket`]), as the
//! workspace whose token the terminal holds.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_json::{json, Value as Json};

/// Say `hello` with `token` at `socket`, then relay each non-empty request
/// line from `input` and write its reply to `output`, until `input` ends.
/// A socket that can't be reached, a refused `hello`, or a window that hangs
/// up is an error.
pub fn relay(socket: &Path, token: &str, input: impl BufRead, mut output: impl Write) -> Result<()> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("can't reach the window's agent socket {}", socket.display()))?;
    let mut writer = &stream;
    let mut reader = BufReader::new(&stream);
    send(&mut writer, &json!({ "op": "hello", "token": token }).to_string())?;
    let hello: Json = serde_json::from_str(&receive(&mut reader)?)?;
    if hello["ok"] != json!(true) {
        bail!("the window refused this terminal's token: {}", hello["error"].as_str().unwrap_or("no reason given"));
    }
    for line in input.lines() {
        let line = line?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        send(&mut writer, line)?;
        output.write_all(receive(&mut reader)?.as_bytes())?;
        output.flush()?;
    }
    Ok(())
}

fn send(writer: &mut impl Write, line: &str) -> Result<()> {
    let mut buf = line.as_bytes().to_vec();
    buf.push(b'\n');
    writer.write_all(&buf).and_then(|()| writer.flush()).context("the window hung up the agent channel")
}

/// The next reply line, newline included.
fn receive(reader: &mut impl BufRead) -> Result<String> {
    let mut line = String::new();
    if reader.read_line(&mut line).context("reading the window's reply")? == 0 {
        bail!("the window hung up the agent channel");
    }
    Ok(line)
}
