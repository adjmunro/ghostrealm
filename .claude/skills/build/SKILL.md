---
name: build
description: Use before running cargo, mise or zig in this repo, or when target/ needs cleaning.
---

- `cargo` lives in `~/.cargo/bin`, which the agent shell's `PATH` lacks: `export PATH="$HOME/.cargo/bin:$PATH"` first. There is no `timeout` binary.
- Zig is pinned in `mise.toml` and is required to build `libghostty-vt-sys` (it compiles Ghostty from source). Run cargo from the repo root so `mise` activates Zig, or prefix `mise exec --`. The scratchpad has no `mise.toml`, so Zig is not on `PATH` there: export it explicitly when building outside the repo.
- `target/` grows without bound (Cargo keeps superseded hashed copies). `mise run clean-stale` frees them. It skips a target dir while a build holds its locks, or when its lock files can't be opened or created (`can't lock …; skipped`), so it is safe to run any time. It refuses (exit 2) when `CARGO_TARGET_DIR` is set without `--target-dir`.
- `mise run clean-all` also runs that cleanup on every other worktree's `target/`. On macOS it also empties target dirs of agent worktrees (`.claude/worktrees/*`), scratchpads and ghostrealm target dirs in the system temp dir once idle (`--idle-hours`, default 2) and no running program lives in them, keeping `CACHEDIR.TAG`, lock files and dirs with other locks; and it removes leftover test and web-host temp dirs. On other systems it empties and removes none of these. An agent worktree awaiting review or fixes loses its `target/` this way, so pass `-- --except <worktree>/target` (repeatable) for each whose `target/` exists (a missing `--except` dir is an error and nothing is cleaned), or leave clean-all to the director.
- Add `-- --dry-run` to either to list what would go; a dry run removes and creates nothing in the dirs it would clean (the mise tasks still build `target/clean-stale`).
- Never share a `CARGO_TARGET_DIR` between checkouts: `target/debug/ghostrealm` and the web bundle beside it are overwritten by whichever checkout built last, and the app finds the web host next to its own executable.
- For a throwaway target dir, set `CARGO_INCREMENTAL=0`: incremental caches only pay off on rebuilds and add gigabytes.
