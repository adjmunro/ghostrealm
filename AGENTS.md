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
- `target/` grows without bound (Cargo keeps superseded hashed copies). `mise run clean-stale -- --dry-run` lists what it would free; drop `--dry-run` to free it. Don't run it during a build.
- `cargo` lives in `~/.cargo/bin`, which the agent shell's `PATH` lacks: `export PATH="$HOME/.cargo/bin:$PATH"` first. There is no `timeout` binary either.
- The web host (`crates/web-host`, CEF) is not a default workspace member: plain `cargo build/test` skip it. Build + bundle it with `mise run web` (CMake + Ninja come from `mise.toml`). Toggling its `bundle` feature rebuilds `cef`, so always build it via that task. Its integration test (`cargo test -p ghostrealm-web --test host`) skips unless the bundle exists.
- The host runs headless (no display needed), so web behaviour is verifiable without a GUI: drive it with `ghostrealm agent` (`web.new`, `web.eval`, `get surface`) or the host test.
- In the agent sandbox `~/.cargo`, `~/.local` and `~/.cache` are read-only, `mise install` can't reach its CDN, and git can't write a `.git/config`. To build: curl Zig's tarball from ziglang.org into `$TMPDIR` and put it on `PATH`; set `CARGO_HOME` under `$TMPDIR`, `CARGO_NET_GIT_FETCH_WITH_CLI=true`, `GIT_CONFIG_GLOBAL=/dev/null` (the global config rewrites GitHub URLs to SSH), and `GHOSTTY_SOURCE_DIR` to a codeload tarball of the pinned Ghostty commit. Zig can't fetch through the proxy: curl each dependency it names and `zig fetch` it from inside the Ghostty directory. In that copy's `src/build/libsystem_override.sh`, replace `xcrun ` with `/Library/Developer/CommandLineTools/usr/bin/` and give `mktemp -d` a `$TMPDIR` template (both default to the unwritable per-user temp dir). There, PTY and GPU tests fail, the CEF host can't start (its profile lock binds a Unix socket), and `git commit` can't sign (no 1Password agent).

## Verifying the app

- The sandbox has no display: a launched `ghostrealm` window never fires winit's `resumed`, so it neither draws nor writes its config — it just idles without error. A "still alive after 2s" smoke check only proves it didn't crash at startup; it does NOT prove rendering. Don't claim the GUI works from that alone.
- Judge performance (scroll, typing, rendering) only from `--release` builds. The default dev profile leaves font shaping (cosmic-text/swash) unoptimised, so a debug build feels slow on its own, which is not a regression.
- On macOS, a test that spawns the debug `ghostrealm` binary waits ~2s before its `main` runs (process load, outside our code). Time from `main` before blaming a spawned server or CLI path for slowness.
- Verify headlessly instead: `cargo test` (core logic, backend PTY/VT, agent protocol, inbox, config) + the offscreen wgpu render test. Drive behaviour through `ghostrealm agent` (JSON on stdin/stdout) or `ghostrealm dump <cmd>` (grid as text). Real visual confirmation needs a human on a desktop session.

## Editing Agents.md

> Agents.md is a record of **mistakes**.

You may edit these Markdown files if:

1. You encounter a recurring tooling issue that obstructs your progress.
2. You took more than 3 turns to discover something non-obvious.
3. You repeatedly mis-interpreted or misunderstood something.
4. Your human had to correct you on a repeated mistake.
5. You have changed something that affects the accuracy of these Markdown files.
