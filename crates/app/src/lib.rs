//! ghostrealm application library.
//!
//! Split into a lib + thin bin so the app logic (state, registry, agent channel)
//! is testable without spawning the process. `main.rs` just calls [`run_cli`].

pub mod agent;
pub mod app_state;
pub mod plugin;
pub mod plugins;
pub mod row_cache;
pub mod tap;
pub mod window;

use std::time::{Duration, Instant};

use anyhow::Result;
use ghostrealm_terminal::{Lifecycle, TerminalBackend};
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal};

/// Dispatch the CLI:
/// - `ghostrealm`                 → open the window (single terminal, for now)
/// - `ghostrealm agent [cmd...]`  → JSON agent channel over stdin/stdout
/// - `ghostrealm dump [cmd...]`   → headless: run a command, print the grid
pub fn run_cli() -> Result<()> {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("agent") => {
            args.remove(0);
            let mut state = app_state::AppState::new();
            if !args.is_empty() {
                state = app_state::AppState::new().with_shell_line(args.join(" "));
            }
            state.new_vtab()?;
            let stdin = std::io::stdin();
            let stdout = std::io::stdout();
            agent::run(stdin.lock(), stdout.lock(), state)
        }
        Some("dump") => {
            args.remove(0);
            let command_line = if args.is_empty() {
                "ls -la / ; echo exit-code:$?".to_string()
            } else {
                args.join(" ")
            };
            dump(&command_line)
        }
        _ => {
            let command_line = if args.is_empty() {
                None
            } else {
                Some(args.join(" "))
            };
            window::run(command_line)
        }
    }
}

fn dump(command_line: &str) -> Result<()> {
    let (cols, rows) = (100u16, 30u16);
    let mut cmd = CommandBuilder::new("/bin/sh");
    cmd.arg("-c");
    cmd.arg(command_line);

    let mut term = GhosttyTerminal::spawn(cols, rows, 8, 16, Some(cmd))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        term.pump();
        if let Lifecycle::Exited(_) = term.lifecycle() {
            std::thread::sleep(Duration::from_millis(30));
            term.pump();
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    let grid = term.snapshot();
    let cols = grid.size.cols as usize;
    println!(
        "title: {}   lifecycle: {:?}",
        term.title().unwrap_or_else(|| "<none>".into()),
        term.lifecycle()
    );
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
    Ok(())
}
