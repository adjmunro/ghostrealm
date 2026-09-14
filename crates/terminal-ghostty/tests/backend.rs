//! Integration tests for the libghostty-vt + portable-pty backend.
//! These spawn real PTYs, so they need Zig on PATH (via mise) to build the
//! native lib. Failures print next steps for a follow-up agent.

use std::time::{Duration, Instant};

use ghostrealm_terminal::{Key, KeyPress, Lifecycle, Mods, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal};

fn sh(cmd: &str) -> CommandBuilder {
    let mut c = CommandBuilder::new("/bin/sh");
    c.arg("-c");
    c.arg(cmd);
    c
}

fn pump_until_exit(term: &mut GhosttyTerminal, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    loop {
        term.pump();
        if let Lifecycle::Exited(_) = term.lifecycle() {
            std::thread::sleep(Duration::from_millis(30));
            term.pump();
            return;
        }
        if Instant::now() > deadline {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn row_text(term: &mut GhosttyTerminal, row: u16) -> String {
    let grid = term.snapshot();
    let mut s = String::new();
    for col in 0..grid.size.cols {
        match grid.cell(col, row) {
            Some(c) if !c.text.is_empty() => s.push_str(&c.text),
            _ => s.push(' '),
        }
    }
    s.trim_end().to_string()
}

#[test]
fn renders_child_output_into_grid() {
    let mut term = GhosttyTerminal::spawn(40, 10, 8, 16, Some(sh("printf 'hello world'")))
        .expect("spawn backend");
    pump_until_exit(&mut term, Duration::from_secs(5));
    let first = row_text(&mut term, 0);
    assert!(
        first.contains("hello world"),
        "expected row 0 to contain 'hello world', got {first:?}.\n\
         Next steps: confirm pump() drained the PTY (reader thread + channel) and that \
         build_grid's cell iteration order is left-to-right; dump the whole grid to inspect."
    );
}

#[test]
fn applies_sgr_colors() {
    // Bright-red "X" via SGR, then reset.
    let mut term =
        GhosttyTerminal::spawn(20, 4, 8, 16, Some(sh("printf '\\033[91mX\\033[0m'")))
            .expect("spawn backend");
    pump_until_exit(&mut term, Duration::from_secs(5));
    let grid = term.snapshot();
    let cell = grid.cell(0, 0).expect("cell (0,0)");
    assert_eq!(
        cell.text, "X",
        "expected 'X' at (0,0), got {:?}. Next steps: verify graphemes() decoding in build_grid.",
        cell.text
    );
    assert_ne!(
        cell.fg, grid.default_fg,
        "expected a non-default foreground for SGR-coloured cell; got default {:?}.\n\
         Next steps: check cell.fg_color() handling and default resolution in build_grid.",
        grid.default_fg
    );
}

#[test]
fn detects_child_exit() {
    let mut term =
        GhosttyTerminal::spawn(20, 4, 8, 16, Some(sh("exit 7"))).expect("spawn backend");
    pump_until_exit(&mut term, Duration::from_secs(5));
    match term.lifecycle() {
        Lifecycle::Exited(code) => assert_eq!(
            code,
            Some(7),
            "expected exit code 7, got {code:?}. Next steps: check child.try_wait() \
             and ExitStatus::exit_code() mapping in lifecycle()."
        ),
        Lifecycle::Running => panic!(
            "child reported Running after `exit 7`. Next steps: ensure the slave fd is dropped \
             after spawn so the child can exit, and that try_wait() is polled."
        ),
    }
}

#[test]
fn encodes_basic_keys() {
    let mut term =
        GhosttyTerminal::spawn(20, 4, 8, 16, Some(sh("sleep 2"))).expect("spawn backend");

    let enter = term
        .encode_key(&KeyPress { key: Key::Enter, mods: Mods::default(), text: None })
        .expect("encode enter");
    assert_eq!(
        enter, b"\r",
        "Enter should encode to CR. Got {enter:?}. Next steps: check map_key(Enter) and encoder \
         options (set_options_from_terminal)."
    );

    let a = term
        .encode_key(&KeyPress {
            key: Key::Char('a'),
            mods: Mods::default(),
            text: Some("a".into()),
        })
        .expect("encode a");
    assert_eq!(a, b"a", "plain 'a' should encode to \"a\". Got {a:?}.");

    let ctrl_c = term
        .encode_key(&KeyPress {
            key: Key::Char('c'),
            mods: Mods { ctrl: true, ..Default::default() },
            text: None,
        })
        .expect("encode ctrl-c");
    assert_eq!(
        ctrl_c,
        vec![0x03],
        "Ctrl+C should encode to 0x03 (ETX). Got {ctrl_c:?}.\n\
         Next steps: if empty/wrong, the encoder may need a physical Key for letters \
         (map Char('c') -> Key::KeyC) rather than only unshifted_codepoint."
    );
}
