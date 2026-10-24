---
name: build
description: Use before running cargo, mise or zig in this repo, or when target/ needs cleaning.
---

- `cargo` lives in `~/.cargo/bin`, which the agent shell's `PATH` lacks: `export PATH="$HOME/.cargo/bin:$PATH"` first. There is no `timeout` binary.
- Zig is pinned in `mise.toml` and is required to build `libghostty-vt-sys` (it compiles Ghostty from source). Run cargo from the repo root so `mise` activates Zig, or prefix `mise exec --`. The scratchpad has no `mise.toml`, so Zig is not on `PATH` there: export it explicitly when building outside the repo.
- `target/` grows without bound (Cargo keeps superseded hashed copies). `mise run clean-stale -- --dry-run` lists what it would free; drop `--dry-run` to free it. Never during a build.
