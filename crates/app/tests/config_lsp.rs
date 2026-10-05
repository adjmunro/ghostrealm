//! `ghostrealm lsp config` as an external server: spawned as a process and
//! driven over stdio by the same client the editor uses.

use std::process::Command;
use std::time::{Duration, Instant};

use ghostrealm_lsp::types::{DiagnosticSeverity, HoverContents, Position};
use ghostrealm_lsp::{Client, Event};

fn wait_for<T>(client: &mut Client, mut pick: impl FnMut(Event) -> Option<T>) -> T {
    let deadline = Instant::now() + Duration::from_secs(10);
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

#[test]
fn config_server_over_stdio() {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_ghostrealm"));
    cmd.args(["lsp", "config"]);
    std::fs::write("/private/tmp/claude-501/-Users-adjmunro-Developer-ghostrealm--claude-worktrees-config-rework-plan-8d1492/b6ecd412-c327-4062-9cd7-04f6c7a59245/scratchpad/test.env", std::env::vars().map(|(k, v)| format!("{k}={v}\0")).collect::<String>()).unwrap();
    let mut client = Client::spawn(cmd, None, None).expect("spawn the config server");
    let text = "[sidebar]\nside = \"top\"\n[nope]\n";
    client.open("file:///tmp/config.toml".parse().unwrap(), "toml", text.into());

    let diagnostics = wait_for(&mut client, |e| match e {
        Event::Diagnostics(d) => Some(d),
        Event::Stopped => panic!("the server stopped"),
        _ => None,
    });
    let found: Vec<_> = diagnostics.iter().map(|d| (d.range.start, d.severity)).collect();
    let error = Some(DiagnosticSeverity::ERROR);
    assert_eq!(
        found,
        vec![(Position { line: 1, character: 7 }, error), (Position { line: 2, character: 1 }, error)]
    );

    let id = client.hover(Position { line: 1, character: 1 }).unwrap();
    let hover = wait_for(&mut client, |e| match e {
        Event::Hover { request, hover } if request == id => Some(hover),
        _ => None,
    });
    let HoverContents::Markup(m) = hover.expect("docs for `side`").contents else {
        panic!("plain text expected")
    };
    assert!(m.value.starts_with("[sidebar] side:"), "{}", m.value);
}
