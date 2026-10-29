---
name: build
description: Use before running cargo, mise or zig in this repo, or when target/ needs cleaning.
---

- `cargo` lives in `~/.cargo/bin`, which the agent shell's `PATH` lacks: `export PATH="$HOME/.cargo/bin:$PATH"` first. There is no `timeout` binary.
- Zig is pinned in `mise.toml` and is required to build `libghostty-vt-sys` (it compiles Ghostty from source). Run cargo from the repo root so `mise` activates Zig, or prefix `mise exec --`. The scratchpad has no `mise.toml`, so Zig is not on `PATH` there: export it explicitly when building outside the repo.
- `target/` grows without bound (Cargo keeps superseded hashed copies). `mise run clean-stale` frees them; it skips a target dir a build holds, so it is safe to run any time. `mise run clean-all` also removes idle target dirs of other worktrees and scratchpads, and leftover test and web-host temp dirs. Add `-- --dry-run` to either to list what would go.
- Never share a `CARGO_TARGET_DIR` between checkouts: `target/debug/ghostrealm` and the web bundle beside it are overwritten by whichever checkout built last, and the app finds the web host next to its own executable.
- For a throwaway target dir, set `CARGO_INCREMENTAL=0`: incremental caches only pay off on rebuilds and add gigabytes.
