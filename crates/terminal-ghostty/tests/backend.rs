//! Integration tests for the libghostty-vt + portable-pty backend.
//! These spawn real PTYs, so they need Zig on PATH (via mise) to build the
//! native lib. Failures print next steps for a follow-up agent.

use std::time::{Duration, Instant};

use ghostrealm_terminal::{Key, KeyPress, Lifecycle, Mods, Scroll, TerminalBackend};
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
fn snapshot_into_reuses_grid_without_leaving_stale_cells() {
    use ghostrealm_terminal::Grid;

    let mut term = GhosttyTerminal::spawn(40, 10, 8, 16, Some(sh("printf 'ABC'")))
        .expect("spawn backend");
    pump_until_exit(&mut term, Duration::from_secs(5));

    // Reuse a grid that was previously filled with different, larger content:
    // the fill must overwrite live cells and blank everything past the child's
    // output — no leftovers from the old grid.
    let mut grid = Grid::blank(
        ghostrealm_terminal::GridSize { cols: 80, rows: 24 },
        [1, 2, 3],
        [4, 5, 6],
    );
    for (i, c) in grid.cells.iter_mut().enumerate() {
        c.text = format!("x{i}");
    }
    term.snapshot_into(&mut grid);

    assert_eq!(
        (grid.size.cols, grid.size.rows),
        (40, 10),
        "snapshot_into should resize the target to the terminal grid"
    );
    assert_eq!(grid.cells.len(), 40 * 10, "cell vector trimmed to the new size");
    assert_eq!(grid.cell(0, 0).unwrap().text, "A");
    assert_eq!(grid.cell(1, 0).unwrap().text, "B");
    assert_eq!(grid.cell(2, 0).unwrap().text, "C");
    assert!(
        grid.cell(3, 0).unwrap().text.is_empty(),
        "cell past the output must be blanked, not a stale 'x…' leftover: {:?}",
        grid.cell(3, 0).unwrap().text
    );

    // A fresh snapshot() and a reuse snapshot_into() must agree.
    let fresh = term.snapshot();
    assert_eq!(fresh.size, grid.size);
    assert_eq!(fresh.cells, grid.cells, "reuse path must match the fresh snapshot");
}

#[test]
fn applies_sgr_colors() {
    // Bright-red "X" via SGR, then reset.
    let mut term = GhosttyTerminal::spawn(20, 4, 8, 16, Some(sh("printf '\\033[91mX\\033[0m'")))
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
fn budgeted_pump_bounds_work_and_reports_backlog() {
    // ~68 KB of output — comfortably more than one reader chunk.
    let mut term = GhosttyTerminal::spawn(
        80,
        24,
        8,
        16,
        Some(sh("yes 0123456789abcdef | head -n 4000; sleep 2")),
    )
    .expect("spawn backend");
    // Let the reader thread accumulate several chunks in the channel.
    std::thread::sleep(Duration::from_millis(250));

    let pumped = term.pump_budgeted(1024);
    assert!(pumped.changed, "a tiny budget should still consume some output");
    assert!(
        pumped.more,
        "a tiny budget against a large backlog must report more work pending"
    );

    // A generous budget then drains the rest.
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        let p = term.pump_budgeted(usize::MAX);
        if !p.changed && !p.more {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let shown = (0..24).any(|r| row_text(&mut term, r).contains("0123456789abcdef"));
    assert!(
        shown,
        "after draining, the viewport should show the repeated output somewhere"
    );
}

#[test]
fn scrollback_viewport_reveals_history() {
    // Print 100 numbered lines into a 10-row grid: lines 1..90 are scrollback.
    let mut term = GhosttyTerminal::spawn(
        40,
        10,
        8,
        16,
        Some(sh("for i in $(seq 1 100); do echo line-$i; done; sleep 2")),
    )
    .expect("spawn backend");

    // Pump until the bottom of the output has landed.
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        term.pump();
        if (0..10).any(|r| row_text(&mut term, r).contains("line-100")) {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let at_bottom = (0..10).any(|r| row_text(&mut term, r).contains("line-100"));
    assert!(at_bottom, "expected the latest output (line-100) at the bottom");

    // Scroll up ~50 lines into history: an earlier line should appear and the
    // latest line leave. (From the tail, this lands the viewport around 42–51.)
    term.scroll(Scroll::Delta(-50));
    let mut early_visible = false;
    let mut latest_gone = true;
    for r in 0..10 {
        let t = row_text(&mut term, r);
        if t.contains("line-46") {
            early_visible = true;
        }
        if t.contains("line-100") {
            latest_gone = false;
        }
    }
    assert!(
        early_visible,
        "scrolling up into history should reveal an earlier line (line-46)"
    );
    assert!(latest_gone, "scrolling up should move the latest line out of view");

    // Jump back to the bottom.
    term.scroll(Scroll::Bottom);
    assert!(
        (0..10).any(|r| row_text(&mut term, r).contains("line-100")),
        "Scroll::Bottom should return to the live tail"
    );

    // And to the very top.
    term.scroll(Scroll::Top);
    assert!(
        (0..10).any(|r| row_text(&mut term, r).contains("line-1")),
        "Scroll::Top should reveal the oldest line"
    );
}

#[test]
fn is_busy_tracks_a_foreground_command() {
    // An interactive shell does job control, so tcgetpgrp on the master reflects
    // the foreground command's process group. `sh -i` reads commands from the PTY.
    let mut c = CommandBuilder::new("/bin/sh");
    c.arg("-i");
    let mut term = GhosttyTerminal::spawn(80, 24, 8, 16, Some(c)).expect("spawn backend");

    // Wait for the shell to settle at its prompt (not busy).
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        term.pump();
        if !term.is_busy() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!term.is_busy(), "shell at its prompt should not be busy");

    // Run a foreground command; the shell puts it in its own process group.
    term.write_bytes(b"sleep 1\n");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut went_busy = false;
    while Instant::now() < deadline {
        term.pump();
        if term.is_busy() {
            went_busy = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        went_busy,
        "a running foreground command should read as busy (needs a job-control shell)"
    );

    // When it finishes, the shell returns to the foreground and we go idle.
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut went_idle = false;
    while Instant::now() < deadline {
        term.pump();
        if !term.is_busy() {
            went_idle = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(went_idle, "after the command finishes the shell should be idle again");
}

#[test]
fn detects_child_exit() {
    let mut term = GhosttyTerminal::spawn(20, 4, 8, 16, Some(sh("exit 7"))).expect("spawn backend");
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
        .encode_key(&KeyPress {
            key: Key::Enter,
            mods: Mods::default(),
            text: None,
        })
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
            mods: Mods {
                ctrl: true,
                ..Default::default()
            },
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
