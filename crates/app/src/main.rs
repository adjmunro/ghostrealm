//! ghostrealm — terminal multiplexer.
//!
//! Until the wgpu shell lands, the binary runs a headless grid dump: spawn a
//! command in a real PTY, drive it through the VT engine, and print the
//! resulting cell grid as text. This is also the seed of the text-based
//! introspection an agent uses to observe the app without a screenshot.

use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_terminal::{Lifecycle, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let command_line = if args.is_empty() {
        "ls -la / ; echo exit-code:$?".to_string()
    } else {
        args.join(" ")
    };

    let (cols, rows) = (100u16, 30u16);
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg(&command_line);

    let mut term = GhosttyTerminal::spawn(cols, rows, 8, 16, Some(cmd))?;

    // Drive until the child exits, or a safety timeout.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        term.pump();
        if let Lifecycle::Exited(_) = term.lifecycle() {
            // One more pump to drain any final output.
            std::thread::sleep(Duration::from_millis(30));
            term.pump();
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    print_grid(&mut term);
    Ok(())
}

fn print_grid(term: &mut GhosttyTerminal) {
    let grid = term.snapshot();
    let title = term.title().unwrap_or_else(|| "<no title>".to_string());
    let cols = grid.size.cols as usize;
    println!("title: {title}   lifecycle: {:?}", term.lifecycle());
    println!("+{}+", "-".repeat(cols));
    for row in 0..grid.size.rows {
        let mut line = String::with_capacity(cols);
        for col in 0..grid.size.cols {
            match grid.cell(col, row) {
                Some(c) if !c.text.is_empty() => line.push_str(&c.text),
                _ => line.push(' '),
            }
        }
        println!("|{}|", line.trim_end());
    }
    println!("+{}+", "-".repeat(cols));
    println!(
        "cursor: ({},{}) visible={}",
        grid.cursor.col, grid.cursor.row, grid.cursor.visible
    );
}
