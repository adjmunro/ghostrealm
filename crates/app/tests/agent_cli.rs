//! `ghostrealm agent` as a process: which channel it takes.

use std::io::Write;
use std::process::{Command, Output, Stdio};

fn agent(args: &[&str], socket: &str, input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ghostrealm"))
        .arg("agent")
        .args(args)
        .env("GHOSTREALM_AGENT_SOCKET", socket)
        .env("GHOSTREALM_AGENT_TOKEN", "0".repeat(64))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ghostrealm agent");
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

/// Given a terminal whose window's socket is gone;
/// when `ghostrealm agent` runs there;
/// then it exits 1, naming the socket, and offers no other way in.
#[test]
fn a_dead_socket_is_an_error_with_no_fallback() {
    let out = agent(&[], "/nonexistent/ghostrealm.sock", "{\"op\":\"pump\"}\n");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(stderr.contains("/nonexistent/ghostrealm.sock"), "{stderr}");
    assert!(!stderr.contains("headless") && !stderr.contains("unset"), "{stderr}");
}

/// Given a terminal in a window;
/// when `ghostrealm agent` runs there with a command of its own;
/// then it exits 1: the window's channel runs none, and no other way in
/// is offered.
#[test]
fn a_command_for_the_windows_channel_is_an_error() {
    let out = agent(&["true"], "/nonexistent/ghostrealm.sock", "");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("takes no arguments") && !stderr.contains("headless"), "{stderr}");
}

/// Given a terminal in a window;
/// when `ghostrealm agent --headless` runs there;
/// then it serves a headless app of its own.
#[test]
fn headless_is_forced_by_its_flag() {
    let out = agent(&["--headless"], "/nonexistent/ghostrealm.sock", "{\"op\":\"pump\"}\n");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(String::from_utf8_lossy(&out.stdout), "{\"ok\":true}\n");
}

/// `ghostrealm agent --mcp` with `args`, outside any window's terminal,
/// fed `input`.
fn mcp_outside_a_window(args: &[&str], input: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ghostrealm"))
        .arg("agent")
        .args(args)
        .env_remove("GHOSTREALM_AGENT_SOCKET")
        .env_remove("GHOSTREALM_AGENT_TOKEN")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ghostrealm agent --mcp");
    child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

const INITIALIZE: &str = "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"protocolVersion\":\"2025-11-25\",\"capabilities\":{},\"clientInfo\":{\"name\":\"t\",\"version\":\"0\"}}}\n";

/// Given a terminal whose window's socket is gone;
/// when `ghostrealm agent --mcp` runs there;
/// then it exits 1 naming the socket, having written nothing on stdout.
#[test]
fn mcp_with_a_dead_socket_is_an_error_with_no_reply() {
    let out = agent(&["--mcp"], "/nonexistent/ghostrealm.sock", INITIALIZE);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    assert!(out.stdout.is_empty(), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(stderr.contains("/nonexistent/ghostrealm.sock") && !stderr.contains("headless"), "{stderr}");
}

/// Given a shell outside any ghostrealm window;
/// when `ghostrealm agent --mcp` runs there, alone, with `--headless`, or
/// with arguments;
/// then each exits 1 with nothing on stdout, and no headless app is served.
#[test]
fn mcp_outside_a_window_is_refused_headless_or_not() {
    for (args, says) in [
        (&["--mcp"][..], "isn't a ghostrealm window's terminal"),
        (&["--headless", "--mcp"][..], "no headless mode"),
        (&["--mcp", "true"][..], "takes no arguments"),
    ] {
        let out = mcp_outside_a_window(args, INITIALIZE);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {stderr}");
        assert!(out.stdout.is_empty(), "{args:?}: {}", String::from_utf8_lossy(&out.stdout));
        assert!(stderr.contains(says), "{args:?}: {stderr}");
    }
}

/// Given a socket that accepts any `hello` and answers each request with a
/// refusal;
/// when `ghostrealm agent --mcp` runs against it and a client initializes,
/// notifies, sends a malformed line and calls a tool;
/// then stdout carries exactly one JSON-RPC line per request, the call a
/// tool error with the socket's reason, and it exits 0 when stdin ends.
#[test]
fn mcp_speaks_json_rpc_lines_on_stdout_alone() {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixListener;
    let dir = std::env::temp_dir().join(format!("gr-mcp-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("s.sock");
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).unwrap();
    let window = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut writer = &stream;
        let mut requests = Vec::new();
        for line in BufReader::new(&stream).lines() {
            let line = line.unwrap();
            let reply = if requests.is_empty() { "{\"ok\":true}" } else { "{\"ok\":false,\"error\":\"refused here\"}" };
            requests.push(line);
            writeln!(writer, "{reply}").unwrap();
        }
        requests
    });
    let input = format!(
        "{INITIALIZE}{}\n{}\n{}\n",
        "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}",
        "{oops",
        "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"set\",\"arguments\":{\"id\":\"app.quit\"}}}",
    );
    let out = agent(&["--mcp"], path.to_str().unwrap(), &input);
    // Unblocks the accept if the server never connected.
    let _ = std::os::unix::net::UnixStream::connect(&path);
    let requests = window.join().unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let stdout = String::from_utf8(out.stdout).unwrap();
    let replies: Vec<serde_json::Value> = stdout.lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 3, "{stdout}");
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "ghostrealm");
    assert_eq!(replies[1]["error"]["code"], -32700);
    assert_eq!(replies[2]["result"]["isError"], true);
    assert_eq!(replies[2]["result"]["content"][0]["text"], "refused here");
    assert_eq!(requests.len(), 2, "{requests:?}");
    assert!(requests[0].contains("\"op\":\"hello\""));
    assert_eq!(serde_json::from_str::<serde_json::Value>(&requests[1]).unwrap(), serde_json::json!({ "op": "set", "id": "app.quit" }));
}
