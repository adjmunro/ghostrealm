# Plan: the web host in the normal build

Plain `cargo build`, `cargo run` and `cargo run --release` produce an up-to-date
CEF web host bundle beside the `ghostrealm` binary. The owner chose option A in
`decisions-pending.md` item 2: the app's `build.rs` builds and bundles the host.

## Mechanism

`crates/app/build.rs` runs a nested cargo build of the host in its own target
dir, bundles it there, and moves the bundle beside the app binary.

1. **Skip conditions** (no nested build, one `cargo:warning` saying why):
   - `GHOSTREALM_NO_WEB` set to anything but empty or `0`.
   - `CARGO_CFG_TARGET_OS` is not `macos` (see Non-macOS).
   - `TARGET != HOST`: the bundler must run on the build machine.
   - rust-analyzer's build-script pass (`RA_RUSTC_WRAPPER` set): its
     `RUSTC_WRAPPER` would leak into the nested build. To verify during
     implementation which variable rust-analyzer sets.
2. **Dirs.** `cargo metadata --no-deps --format-version 1` gives
   `target_directory`; `OUT_DIR`'s fourth ancestor gives the profile dir name
   (`debug`, `release`, or a custom profile) and, when its parent is named
   `$TARGET`, the triple. The app's artifact dir is
   `<target_directory>/[<triple>/]<profile dir>`. Reading `target_directory`
   from metadata instead of walking up `OUT_DIR` keeps this right when
   `build.build-dir` is split from the target dir. The nested target dir is
   `<target_directory>/web-host`.
3. **Nested build.** From the workspace root:
   `$CARGO build --locked -p ghostrealm-web-host --features bundle --bins
   --target-dir <target>/web-host [--release]`.
   - Profile: `--release` when `PROFILE=release`, else dev. Bundles land in
     `target/web-host/{debug,release}`.
   - Env: removes `RUSTC_WORKSPACE_WRAPPER` (outer `cargo clippy` would run
     clippy on the host and change its fingerprints). Keeps `CARGO_MAKEFLAGS`
     so the nested cargo shares the outer jobserver, and `CARGO_ENCODED_RUSTFLAGS`
     so flags match the outer build.
   - A separate target dir avoids the build-dir lock deadlock. Checked in a
     scratch workspace on cargo 1.99.1: a nested build that downloads a crate
     while the outer build runs completes.
4. **Bundle.** Runs `target/web-host/<profile>/ghostrealm-web-bundle`, which
   (unchanged) writes `target/web-host/<profile>/ghostrealm-web-host.app`.
5. **Place.** Moves that bundle to `<artifact dir>/ghostrealm-web-host.app`
   by rename, with the guards in Safety.
6. **Rerun triggers**, emitted on every run:
   - `rerun-if-changed`: `crates/web-host`, `crates/web`, `crates/process`
     (a dependency of `crates/web`), `Cargo.lock`, the root `Cargo.toml`
     (profiles), `.cargo/config.toml`, `mise.toml` (CMake/Ninja pins).
   - `rerun-if-changed` on the placed host executable, only after a placement
     succeeds: deleting the bundle (for example `clean-all` emptying a
     worktree) rebuilds it on the next build.
   - `rerun-if-env-changed`: `GHOSTREALM_NO_WEB`, `CEF_PATH`.
   - A test asserts the path list covers every path dependency of
     `ghostrealm-web-host` from `cargo metadata`.

Cost of a rerun: Cargo recompiles a crate whenever its build script reruns
(checked on 1.99.1). Each host source change therefore also recompiles the
`ghostrealm` crate. Rerun triggers are kept to real inputs for this reason. A
helper crate does not avoid it: `cargo run` builds only the bin's package and
its dependencies, and a dependency's rebuild dirties the app the same way.

Layout: the build logic lives in `crates/app/build/web_host.rs`, included by
`build.rs` and by `crates/app/tests/web_host_build.rs` (`#[path]`), so its
unit tests run under `cargo test -p ghostrealm` without CEF.

## How the app finds it

Unchanged. `find_host` looks beside `current_exe()`: a debug build finds
`target/debug/ghostrealm-web-host.app` (dev host), a release build
`target/release/ghostrealm-web-host.app` (release host). `$GHOSTREALM_WEB_HOST`
and `[web] host` stay as overrides ahead of it. The test helpers (`real_host`
in `crates/app/src/plugins/web/mod.rs`, `host_exe` in
`crates/web/tests/common/mod.rs`) look in `target/<profile>` too.

The build script sets `cargo:rustc-env=GHOSTREALM_WEB_BUILD=<reason>` when it
skips, so "web host not found" names the reason (for example
"this build set GHOSTREALM_NO_WEB").

## Safety (placement replaces a bundle this change may not have created)

Rule: any error, missing, empty, truncated or unreadable input means keep the
existing file and fail the step with a message naming the path.

The placement trusts these inputs:

| Input | How it can be wrong | Guard |
| --- | --- | --- |
| Artifact dir from metadata + `OUT_DIR` | metadata fails; unexpected `OUT_DIR` shape; dir missing | Fail without touching anything unless the dir exists, is a directory, and its name equals `OUT_DIR`'s profile dir name |
| Existing `ghostrealm-web-host.app` | a symlink (into a real app elsewhere); a plain file; another app; another user's | Replace only a real directory whose `Contents/Info.plist` reads and has `CFBundleIdentifier` = `dev.ghostrealm.web-host`, owned by the uid that owns `OUT_DIR` |
| `Info.plist` | missing, unreadable, empty, truncated, other id | Keep |
| Staged bundle | bundler failed part way | Place only if it has the host executable, the helper apps and the CEF framework; else keep the old bundle |
| Leftover `.ghostrealm-web-host.app.old` from a killed run | not ours | Remove only if it passes the same ownership check; else keep and fail |
| Renames | cross-device, permissions | Rename old to `.old`, rename staged in; if that fails, rename `.old` back |
| Another OS | Linux layout differs; this machine can't run it | Placement is macOS-only (Non-macOS) |

Tests, each driving one guard to fail and asserting the existing bundle (and
any symlink target) is kept byte for byte:

- dest is a symlink to a dir holding a valid plist: link and target kept;
- dest is a regular file;
- dest has no `Info.plist`; an empty one; a truncated one; one with mode `000`;
  one with another `CFBundleIdentifier`;
- dest owned by another uid (the check takes the uid as a parameter, since a
  test can't chown);
- staged bundle missing its framework: old kept;
- artifact dir read-only, so the rename in fails: old restored;
- `.old` leftover that isn't ours: kept, step fails;
- artifact dir name not matching the profile dir name: nothing touched;
- happy paths: no dest; dest ours (replaced, `.old` removed); `.old` leftover
  ours (removed).

A running host keeps its open files after a swap; helpers it launches later
come from the new bundle. `mise run web` has the same exposure today.

## Failures

Recommended: a failed nested build or bundle fails the app build. The error
prints the last lines of `target/web-host/build.log`, the log's path, and the
fix: run cargo from the repo root so mise supplies CMake and Ninja (checked on
`PATH` after a failure, and named if missing), or set `GHOSTREALM_NO_WEB=1`.

- A failed build script reruns on the next build, so retrying is free.
- Offline matters only for the first build and CEF version bumps; the CEF
  download is cached in the nested `cef-dll-sys` `OUT_DIR`.
- Missing mise tools: plain `cargo build` already needs Zig from the same
  `mise.toml`, so a shell where Zig works has CMake and Ninja too. The
  sandboxed-build setup (Zig from ziglang.org) has neither and needs
  `GHOSTREALM_NO_WEB=1`.

The alternative, a warning and an app without a browser, needs a retry
trigger (`rerun-if-changed` on a missing file). That recompiles the
`ghostrealm` crate on every build until the host builds. Owner question 1.

## Progress during the first build

Cargo captures build-script output; the progress bar sits on
`ghostrealm(build)` for the whole host build (CEF download, ~250 MB, then the
C++ wrapper and the `cef` crate: several minutes).

Plan: the nested build runs with `-vv` and writes everything to
`target/web-host/build.log`. If `/dev/tty` opens, the build script writes a
start line ("building the web host; first build downloads CEF, ~250 MB; log:
…"), the nested `Compiling` lines, a line when `cef-dll-sys` starts its
download, and a finish line with the time. No tty (CI, IDE): only the log and
Cargo's bar. Lines may interleave with Cargo's redrawn bar. Owner question 4.

## Tests (`cargo test -p ghostrealm`)

- The test build runs the same build script with `PROFILE=debug`, so it shares
  `target/web-host/debug` and `target/debug/ghostrealm-web-host.app` with
  `cargo build`. A checkout's first `cargo test` pays the full host build;
  later runs pay nothing unless host inputs changed.
- With the bundle present, the real-host tests in the app and in
  `crates/web/tests` run instead of skipping. They fail under the agent
  sandbox (sandboxed-build skill).
- `GHOSTREALM_NO_WEB=1` skips the host build. The real-host test helpers
  (`real_host`, `host_exe`) also return `None` when it is set, so tests skip
  even when an older bundle exists. Toggling the variable reruns the build
  script and recompiles the app crate once.
- Per agent worktree, the first build costs the CEF download and
  ~1.3 GB (`cef-dll-sys` out ~600 MB, bundle ~330 MB, `cef` crate
  artifacts). Whether agents set `GHOSTREALM_NO_WEB=1` by default is owner
  question 6.

## `mise run web`, `clean-stale`, `clean-all`

- `mise run web`: removed; `cargo build` does its job. Messages, docs and skills
  that name it switch to `cargo build` (app not-found and protocol-mismatch
  messages, `[web] host` help, `Cargo.toml` comment, `docs/SPEC.md`, the
  web-host skill, test skip messages).
- The web-host skill also records: building `ghostrealm-web-host` directly in
  `target/` (`-p ghostrealm-web-host` or `--workspace`) compiles a second CEF
  there; build it through `cargo build`.
- `clean-stale`: `clean_target(target)` also cleans `<target>/web-host` as a
  target dir of its own (its `debug`/`release` locks, the same `Cargo.lock`),
  skipped while a build holds it. This also removes a superseded CEF download
  after a CEF bump.
- `clean-all` emptying an idle agent worktree: today `web-host` would be kept
  whole (it has lock files one level down). It becomes a nested target:
  emptied under its own locks, keeping its `CACHEDIR.TAG` and lock files;
  kept if its locks can't be taken. `last_active` counts its profile dirs.
- New tests (`rustc --test scripts/clean-stale.rs`): superseded units in
  `web-host/debug` are removed; a held `web-host` lock skips only `web-host`;
  emptying empties `web-host` and keeps its tag and lock files; a held
  `web-host` lock during emptying keeps `web-host` whole.
- Doc comments in `mise.toml` and `clean-stale.rs`, and the build skill, name
  `target/web-host`.

## Non-macOS

The host builds and bundles on Linux too (`bundle.rs` has a Linux path), but
this machine can't run it and the Linux bundle is a plain directory, so the
build script skips on every OS but macOS. `docs/TODO.md` gets an entry: build
and place the Linux bundle from `build.rs`, with its own ownership guard.
`build/web_host.rs`'s placement code is `#[cfg(unix)]` (it reads uids); the
build script compiles to a no-op elsewhere. Compile-check the Linux side per
the build skill's `target_os` swap; Windows can't be compile-checked here.

## The `cef_sandbox` link

It lands in `crates/web-host`, not in the build script: the `sandbox` feature
on the `cef` dependency in `crates/web-host/Cargo.toml` (always on, per
DECISION.md, so no CLI feature flag), `libcef_sandbox.dylib` copied into the
bundle by `bundle.rs`, and the helper's sandbox entry in `helper.rs`. The build
script's rerun triggers already cover those files, and the bundle check in
Safety gains `libcef_sandbox.dylib` then.

## Steps (commits)

1. `build(app)`: `build/web_host.rs` (dirs, skip rules, placement with
   guards) and its tests.
2. `build(app)`: `build.rs` runs the nested build, bundle and placement;
   progress and log; `GHOSTREALM_WEB_BUILD` in the not-found message.
3. `test(web)`: real-host helpers skip under `GHOSTREALM_NO_WEB`.
4. `build(scripts)`: clean-stale and clean-all cover `target/web-host`.
5. `build`: remove the `web` mise task; messages, docs and skills name
   `cargo build`; `docs/TODO.md` Linux entry; drop the TODO entry and
   decisions-pending item 2 once done.

Verification beyond unit tests:

- Fresh worktree `cargo build`: bundle in `target/debug`; `ghostrealm agent`
  `web.new` + `get surface` loads a page. Record first-build and no-op times.
- Second `cargo build`: build script doesn't rerun.
- `touch crates/web-host/src/main.rs; cargo build`: host rebuilt, bundle
  replaced.
- `cargo build --release`: release bundle in `target/release`.
- `GHOSTREALM_NO_WEB=1 cargo build`: skipped, warning shown, existing bundle
  untouched.
- First build with `https_proxy=http://127.0.0.1:9` on an empty
  `target/web-host`: fails with the log tail and the hints.
- `cargo test -p ghostrealm`; `rustc --test scripts/clean-stale.rs`;
  `mise run clean-all -- --dry-run`.

## Risks

- rust-analyzer's variable name (step 1 skip) is unverified.
- Each host input change recompiles the app crate (see Mechanism).
- `target/debug` keeps the `cef`/`cef-dll-sys` units `mise run web` built
  there (~1.8 GB in the main checkout): `clean-stale` keeps the newest copies
  of a unit, and nothing builds newer ones. Owner question 7.
- `cargo install --path crates/app` installs no bundle (out of scope).
- Debug and release each download CEF (owner question 3).

## Open questions for the owner

1. Failure: fail the app build (recommended), or warn and build the app without
   a browser (each later build retries and recompiles the app crate)?
2. Host profile: match the app's (recommended; debug and release each build the
   host and keep a CEF copy), or always release (one copy; a debug app runs a
   release host)?
3. Share one CEF download across profiles and worktrees through `CEF_PATH` (a
   cache outside `target/`, saves ~600 MB per copy, needs its own cleanup of
   old versions), or keep it inside each target dir (recommended for now:
   `clean-stale` already handles it)?
4. Progress on `/dev/tty` during the host build (recommended), or only Cargo's
   bar and the log?
5. Remove `mise run web` (recommended), or keep it as an alias for
   `cargo build`?
6. Should agents build with `GHOSTREALM_NO_WEB=1` unless their task touches
   web (saves ~1.3 GB and minutes per worktree; web code then goes untested
   there)?
7. The old `cef`/`cef-dll-sys` units in `target/debug` (~1.8 GB): remove them by
   hand once, or teach `clean-stale` that those packages no longer belong in
   the outer target dir?
