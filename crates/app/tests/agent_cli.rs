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
