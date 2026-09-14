//! Speed guards for the hot path. Lenient thresholds (CI machines vary); they
//! exist to catch gross regressions and to print numbers with `--nocapture`.

use std::time::{Duration, Instant};

use ghostrealm_terminal::TerminalBackend;
use ghostrealm_terminal_ghostty::{CommandBuilder, GhosttyTerminal};

fn sh(cmd: &str) -> CommandBuilder {
    let mut c = CommandBuilder::new("/bin/sh");
    c.arg("-c");
    c.arg(cmd);
    c
}

#[test]
fn snapshot_throughput() {
    // Fill the grid and scrollback with output, then measure snapshot cost.
    let mut term = GhosttyTerminal::spawn(
        80,
        24,
        8,
        16,
        Some(sh("yes 0123456789abcdef | head -n 5000; sleep 3")),
    )
    .expect("spawn");

    // Pump until the top row has content.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        term.pump();
        let g = term.snapshot();
        if g.cell(0, 0).map(|c| !c.text.is_empty()).unwrap_or(false) {
            break;
        }
        if Instant::now() > deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    let n = 500;
    let start = Instant::now();
    for _ in 0..n {
        let _ = term.snapshot();
    }
    let per = start.elapsed() / n;
    println!("snapshot: {:?}/call ({} cols x 24 rows)", per, 80);
    assert!(
        per < Duration::from_millis(8),
        "snapshot too slow: {per:?}/call. Next steps: reuse RenderState/iterators across calls \
         instead of allocating them each snapshot; avoid per-cell String allocation."
    );
}
