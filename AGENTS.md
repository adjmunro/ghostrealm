# Agents.md

> For repository-level consistency, these files take priority over and/or override any global instructions that may also be on the user's machine.

## Technical Writing
- Cut puffery & exposition.
- Say what it does, not how it feels.
- Never write in code comments or documentation what something _used_ to do (that belongs in git commit bodies!). If I cared how you got there, I would check the git history. What I care about is the reason _why_ it's in the current state / why it is necessary to function the way it does, without any historical reference.

## Progressive Disclosure

- Before high-level planning, read [`GOAL.md`](docs/GOAL.md), [`SPEC`](docs/SPEC.md), and [`DECISION`](docs/DECISION.md).
- For agreed terminology, read [`GLOSSARY.md`](docs/GLOSSARY.md).
- Before adding or changing a surface kind (terminal, editor, file browser, ...), read [`PLUGINS.md`](docs/PLUGINS.md).
- Before mutating source control, read [`GIT.md`](docs/GIT.md)!

## Build & Toolchain

- Zig is pinned in `mise.toml` and is **required** to build `libghostty-vt-sys` (it compiles Ghostty from source). Run cargo from the repo root so `mise` activates Zig, or prefix `mise exec --`. The scratchpad has no `mise.toml`, so Zig is NOT on PATH there — export it explicitly if building outside the repo.
- Do not bump Zig blindly: its major.minor must equal the pinned Ghostty's `build.zig.zon` `minimum_zig_version`, and it must be new enough to link this host's macOS SDK. See [`docs/libghostty-findings.md`](docs/libghostty-findings.md) → "Build toolchain".
- `libghostty-vt` comes from git (pinned `rev`), not crates.io — the published crate is stale.
- `cargo` lives in `~/.cargo/bin`, which the agent shell's `PATH` lacks: `export PATH="$HOME/.cargo/bin:$PATH"` first. There is no `timeout` binary either.

## Verifying the app

- The sandbox has no display: a launched `ghostrealm` window never fires winit's `resumed`, so it neither draws nor writes its config — it just idles without error. A "still alive after 2s" smoke check only proves it didn't crash at startup; it does NOT prove rendering. Don't claim the GUI works from that alone.
- Verify headlessly instead: `cargo test` (core logic, backend PTY/VT, agent protocol, inbox, config) + the offscreen wgpu render test. Drive behaviour through `ghostrealm agent` (JSON on stdin/stdout) or `ghostrealm dump <cmd>` (grid as text). Real visual confirmation needs a human on a desktop session.

## Editing Agents.md

> Agents.md is a record of **mistakes**.

You may edit these Markdown files if:

1. You encounter a recurring tooling issue that obstructs your progress.
2. You took more than 3 turns to discover something non-obvious.
3. You repeatedly mis-interpreted or misunderstood something.
4. Your human had to correct you on a repeated mistake.
5. You have changed something that affects the accuracy of these Markdown files.
