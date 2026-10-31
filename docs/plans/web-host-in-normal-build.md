# Plan: the web host in the normal build

Plain `cargo build`, `cargo run` and `cargo run --release` produce an up-to-date
CEF web host bundle beside the `ghostrealm` binary. The owner chose option A in
`decisions-pending.md` item 2: the app's `build.rs` builds and bundles the host.

Settled (plan review): a failed host build fails the app build; the host
profile matches the app's; CEF stays inside each target dir (no shared
`CEF_PATH`); progress goes to `/dev/tty` when it is safe; `mise run web` is
removed; agents build with `GHOSTREALM_NO_WEB=1` unless their task touches
web, through the skills; old CEF units go through an opt-in age rule in
`clean-stale`.

## Mechanism

`crates/app/build.rs` runs a nested cargo build of the host in its own target
dir, bundles it there, and moves the bundle beside the app binary. The logic
lives in `crates/app/build/web_host.rs`, included by `build.rs` and by
`crates/app/tests/web_host_build.rs` (`#[path]`), so its unit tests run under
`cargo test -p ghostrealm` without CEF.

### 1. Skip conditions

Each skip prints one `cargo:warning` saying why, and sets
`cargo:rustc-env=GHOSTREALM_WEB_BUILD=<reason>` so the app's "web host not
found" names it. Each depends only on inputs Cargo tracks, so a skip never
outlives its cause:

| Skip when | Tracked by |
| --- | --- |
| `GHOSTREALM_NO_WEB` set to anything but empty or `0` | `rerun-if-env-changed=GHOSTREALM_NO_WEB` |
| rust-analyzer's build-script pass (`RA_RUSTC_WRAPPER` set) | `rerun-if-env-changed=RA_RUSTC_WRAPPER` |
| `CARGO_CFG_TARGET_OS` is not `macos` (see Non-macOS) | the target is part of the run unit |
| `TARGET != HOST` (the bundler runs on the build machine) | the target is part of the run unit |
| `OUT_DIR` not under the metadata build dir, or of another shape (see 2) | `OUT_DIR` is part of the run unit; `rerun-if-env-changed` for `CARGO_TARGET_DIR`, `CARGO_BUILD_TARGET_DIR`, `CARGO_BUILD_BUILD_DIR` |

A `cargo metadata` failure is not a skip (it has no tracked input): it fails
the build with metadata's stderr.

Cost of the rust-analyzer skip: `cargo check`, `build`, `test` and `doc`
share one build-script run unit, so each rust-analyzer build-script pass
(workspace load, manifest change) flips the variable. The next terminal build
then reruns the script (nested no-op plus a stamp check, see 5) and recompiles
the `ghostrealm` crate once.

### 2. Dirs

`cargo metadata --no-deps --format-version 1`, run from the workspace root
(inherits `CARGO_TARGET_DIR` and config), gives `target_directory` and
`build_directory`. Without `build_directory` (Cargo older than its
stabilization; the workspace `rust-version` is 1.88) the build dir is
`target_directory`. Both are canonicalized, as is `OUT_DIR`.

- `OUT_DIR` must lie under the build dir. Relative to it, it must be
  `<profile>/build/<pkg>-<hash>/out` or `<triple>/<profile>/build/<pkg>-<hash>/out`
  with `<triple>` = `$TARGET`. Anything else is a skip with: "the build script
  can't see a command-line `--target-dir` or `--config build.target-dir`, and
  resolves a relative `CARGO_TARGET_DIR` from the workspace root; set
  `CARGO_TARGET_DIR` to an absolute path instead". This catches
  `cargo build --target-dir X`, rust-analyzer with `cargo.targetDir: true`,
  and `cargo install`, none of which may write into `<workspace>/target`
  without holding its lock.
- A test in `crates/app/tests/web_host_build.rs` runs the shape check on its
  own `env!("OUT_DIR")` with real metadata and asserts it is not a skip, so a
  toolchain whose layout changed fails a test instead of silently skipping
  every build.
- The app's artifact dir is `<target_directory>/[<triple>/]<profile>`. It
  must exist as a real directory.
- The nested target dir is `<target_directory>/web-host`.

### 3. Nested build

From the workspace root:

```
$CARGO build --locked -p ghostrealm-web-host --features bundle --bins \
  --target-dir <target>/web-host --config 'build.build-dir="<target>/web-host"' \
  [--target <triple>] --profile <p> --message-format json-render-diagnostics
```

- `--config build.build-dir`: `--target-dir` doesn't override a
  `build.build-dir` inherited from `CARGO_BUILD_BUILD_DIR` or config. Without
  it the nested cargo locks the outer build dir and hangs (reproduced in
  review). A test asserts the produced command carries both
  `--target-dir` and `build.build-dir` pointing at `<target>/web-host`.
- `--target <triple>` exactly when the outer `OUT_DIR` shape has a triple
  (an inherited `CARGO_BUILD_TARGET` or `build.target`). The host, helper and
  bundler executable paths come from the nested `compiler-artifact` JSON
  messages (`executable`), not from a guessed layout.
- `<p>` is `dev` when the profile dir is `debug`, else the dir name
  (`release`, or a custom profile's own name).
- Env, by a tested filter function. Removed:
  - set by Cargo for build scripts: `OUT_DIR`, `TARGET`, `HOST`, `PROFILE`,
    `OPT_LEVEL`, `DEBUG`, `NUM_JOBS`, `RUSTC_LINKER`, `CARGO_PKG_*`,
    `CARGO_MANIFEST_*`, `CARGO_CFG_*`, `CARGO_FEATURE_*`, `DEP_*`,
    `CARGO_PRIMARY_PACKAGE`, `CARGO_ENCODED_RUSTFLAGS`,
    `DYLD_FALLBACK_LIBRARY_PATH`;
  - wrappers the outer command sets: `RUSTC_WRAPPER`, `RUSTC_WORKSPACE_WRAPPER`
    (clippy, rust-analyzer). The nested cargo still reads a wrapper from
    config.

  Kept: `CARGO_MAKEFLAGS` (the nested cargo shares the outer jobserver),
  `RUSTFLAGS` and `CARGO_*` config variables (the nested cargo computes the
  same flags from them), `CARGO_NET_OFFLINE`. A command-line `--offline` or
  `--frozen` is not visible to build scripts; offline builds set
  `CARGO_NET_OFFLINE=true` (or `GHOSTREALM_NO_WEB=1`). The skills say so.
- A separate target dir avoids the build-dir lock deadlock. Checked in a
  scratch workspace on cargo 1.99.1: a nested build that downloads a crate
  while the outer build runs completes.
- Output: everything (`-vv` diagnostics and JSON) goes to
  `target/web-host/<profile dir>/ghostrealm-build.log`. Progress: see
  Progress.

### 4. Rerun triggers

Emitted on every run, skips included.

- `rerun-if-changed` on `src`, `Cargo.toml` and any `build.rs` of each path
  package in `ghostrealm-web-host`'s dependency closure: `crates/web-host`,
  `crates/web`, `crates/process`. Not `tests/` or `benches/`. Plus
  `Cargo.lock`, the root `Cargo.toml` (profiles), `.cargo/config.toml` and
  `mise.toml` (CMake and Ninja pins). Only paths that exist are emitted: a
  missing `rerun-if-changed` path reruns the script on every build.
- A test computes the closure from `cargo metadata --no-deps` (declared `path`
  dependencies, no network). It asserts that every lib, bin and build-script
  target's `src_path` of each package is under a watched path, and that each
  manifest is watched.
- `rerun-if-env-changed` for each variable the nested build scripts track
  (`MACOSX_DEPLOYMENT_TARGET`, `CC`, `CXX`, `CFLAGS`, `SDKROOT`, CMake
  variables, `CEF_PATH`, ...). For each `build-script-executed` message the
  script reads `<out_dir>/../output` and takes both forms,
  `cargo:rerun-if-env-changed=` (`cc` 1.4.6 uses it) and
  `cargo::rerun-if-env-changed=`. Names the filter removes are left out. A
  parser test feeds both forms.
- `rerun-if-changed` on the placed host executable, whenever `<dest>` is
  current at the end of the run (after a placement or on a stamp match), so a
  deleted bundle is rebuilt on the next build. A run that omits it drops the
  trigger. Cargo compares the file against the run's start, and the placed
  executable was linked during an earlier or the current run. So before
  emitting, the script makes sure that file's mtime is at most its own start
  time minus 5 s, setting it with `File::set_modified` if not. If setting it
  fails, the script warns and doesn't emit the trigger (the bundle works;
  only deletion detection is lost until the next rerun). Tests: the placed
  file's mtime ends at least 5 s before the recorded start; the nested
  host and helper executables' mtimes are unchanged by it (so the stamp and
  the nested cargo's fingerprints stay valid).

Cargo recompiles a crate whenever its build script reruns (checked on 1.99.1).
Each host input change therefore also recompiles the `ghostrealm` crate.

### 5. Bundle and place

1. Up-to-date check: the placed bundle holds
   `Contents/Resources/ghostrealm-stamp`: the mtime (ns) and size of the nested
   host, helper and bundler executables it was made from, and `BUNDLE_RULES`,
   a version constant in `build/web_host.rs` bumped whenever its completeness
   rules or placement layout change (for example adding
   `libcef_sandbox.dylib`). A change to `bundle.rs` or `BUNDLE_ID` relinks the
   bundler, so its mtime covers them. The stamp matches only when `<dest>` is
   also ours. If it matches, bundling and placement are skipped, and only a
   leftover `.old` is cleaned (step 3 under Safety, if ours; one that isn't is
   kept with a warning, since the placed bundle is current). A clippy rerun or a rust-analyzer flip then costs only a
   nested no-op build. Tests: a bundler-only change, and a `BUNDLE_RULES`
   change, each mismatch the stamp and rebundle.
2. Bundle: `target/web-host/<profile dir>/ghostrealm-web-bundle` (unchanged)
   writes `target/web-host/<profile dir>/ghostrealm-web-host.app`, the
   staging copy. The stamp is written into it.
3. Place it at `<artifact dir>/ghostrealm-web-host.app`, in the order under
   Safety.

## How the app finds it

Unchanged. `find_host` looks beside `current_exe()`: a debug build finds
`target/debug/ghostrealm-web-host.app` (dev host), a release build
`target/release/ghostrealm-web-host.app` (release host). `$GHOSTREALM_WEB_HOST`
and `[web] host` stay as overrides ahead of it.

## Safety (placement replaces a bundle this change may not have created)

Rule: any error, missing, empty, truncated or unreadable input means keep the
existing file and fail the step with a message naming the path.

"Ours" means all of these:
- a real directory (`symlink_metadata`, no symlink);
- owned by the uid that owns `OUT_DIR`;
- `plutil -extract CFBundleIdentifier raw -o - <app>/Contents/Info.plist`
  exits 0 and prints exactly `dev.ghostrealm.web-host`.

Order:

1. The staged bundle is ours and complete (host executable, helper apps, CEF
   framework, stamp); else fail, touching nothing in the artifact dir.
2. If `<dest>` is missing and `.ghostrealm-web-host.app.old` is ours: rename
   `.old` back to `<dest>` (a killed run left it).
3. If `.old` exists: remove it if ours; else fail.
4. If `<dest>` exists: it must be ours, else fail; rename it to `.old`.
5. Rename staged to `<dest>`. On failure, rename `.old` back to `<dest>` and
   fail.
6. Set the mtime of `<dest>`'s host executable (Rerun triggers). Remove
   `.old`; if that fails, warn and leave it for step 3 of the next run.

Removing a bundle (`.old`) deletes its `Info.plist` last, so a removal that
fails part way leaves a bundle the next run still recognises as ours.

The placement trusts these inputs:

| Input | How it can be wrong | Guard |
| --- | --- | --- |
| Artifact dir | metadata fails; `OUT_DIR` outside the build dir or of another shape; dir missing or a symlink | Skip (2) or fail before touching anything |
| Existing `<dest>` | a symlink (into a real app elsewhere); a plain file; another app; another user's | Replace only if ours |
| `Info.plist` | missing, unreadable, empty, truncated, other id | `plutil` fails or prints another id: keep |
| Staged bundle | bundler failed part way | Place only if complete |
| `.old` leftover | not ours | Keep and fail |
| Renames | permissions, cross-device | Restore `.old` on a failed second rename |
| Another OS | Linux layout differs; can't run here | Placement is macOS-only |

Tests, each driving one guard to fail and asserting the existing bundle (and
any symlink target) is kept byte for byte:

- `<dest>` is a symlink to a dir holding a valid plist: link and target kept;
- `<dest>` is a regular file;
- `<dest>` has no `Info.plist`; an empty one; a truncated one; mode `000`; one
  with another `CFBundleIdentifier`;
- `<dest>` owned by another uid (the check takes the uid as a parameter, since
  a test can't chown);
- the staged bundle lacks its framework: `<dest>` kept;
- the second rename fails (staged's parent dir made read-only, so renaming it
  out fails while `<dest>` → `.old` succeeds): `<dest>` restored;
- `.old` not ours: kept, step fails;
- `OUT_DIR` not under the build dir, or of another shape: nothing touched;
- happy paths: no `<dest>`; `<dest>` ours (replaced, `.old` removed); `.old`
  ours with `<dest>` missing (restored, then replaced); matching stamp
  (`<dest>` untouched, a leftover `.old` that is ours removed, the
  placed-exe trigger still emitted).

A running host keeps its open files after a swap. Helpers it launches later
come from the new bundle.

## Failures

A failed nested build or bundle fails the app build. The error prints the last
lines of the profile's `ghostrealm-build.log`, its path, and the fix: run cargo
from the repo root so mise supplies CMake and Ninja (checked on `PATH` after a
failure, and named if missing), or set `GHOSTREALM_NO_WEB=1`.

- A failed build script reruns on the next build, so retrying costs nothing
  extra.
- Offline matters only for the first build and CEF version bumps; the CEF
  download is cached in the nested `cef-dll-sys` `OUT_DIR`.
- Missing mise tools: plain `cargo build` already needs Zig from the same
  `mise.toml`, so a shell where Zig works has CMake and Ninja too. The
  sandboxed-build setup (Zig from ziglang.org) has neither; it sets
  `GHOSTREALM_NO_WEB=1`.

## Progress during the first build

Cargo captures build-script output; its bar sits on `ghostrealm(build)` for the
whole host build (CEF download, ~250 MB, then the C++ wrapper and the `cef`
crate: several minutes).

The script writes to `/dev/tty`: a start line ("building the web host; first
build downloads CEF, ~250 MB; log: …"), the nested `Compiling` lines, a line
when `cef-dll-sys` starts, and a finish line with the time. Lines may
interleave with Cargo's redrawn bar.

- No tty: the log and Cargo's bar only. Checked: the Claude Code desktop app's
  shell has no controlling terminal (`/dev/tty`: "device not configured"), and
  neither has an editor started from the Dock.
- Skipped even with a tty when `CLAUDECODE`, `CI` or `RA_RUSTC_WRAPPER` is
  set, or `TERM` is empty or `dumb`. That covers the Claude Code CLI run from
  a terminal (it sets `CLAUDECODE` in its shells), whose screen tty writes
  would corrupt.
- Remaining exposure: an editor started from a terminal runs `cargo check` on
  save with that terminal as its tty. Its lines land in that terminal.

## Costs by command

- `cargo build`, `run`, `test`, `check`, `doc` (one run unit per profile): the
  first pays the host build; later runs pay nothing until a host input
  changes, then the host rebuild, a re-bundle and an app recompile, once.
- `cargo clippy` (its own run unit): reruns once per host input change. The
  nested build is a no-op (wrappers removed) and the stamp matches, so the cost
  is the nested no-op plus clippy's own app pass.
- rust-analyzer on save (`cargo check`, shared unit): saving a host input
  rebuilds the host and re-bundles in the background while holding
  `target/debug`'s lock; a terminal build waits for it.
- rust-analyzer's build-script pass: a skip, and one extra rerun plus app
  recompile on the next terminal build (Skip conditions).

## Tests (`cargo test -p ghostrealm`)

- The test build runs the same build script, so unless `GHOSTREALM_NO_WEB` is
  set it builds CEF the first time in a checkout. It shares
  `target/web-host/debug` and `target/debug/ghostrealm-web-host.app` with
  `cargo build`.
- With the bundle present, the real-host tests in the app and in
  `crates/web/tests` run instead of skipping. They fail under the agent
  sandbox (sandboxed-build skill).
- The real-host test helpers (`real_host` in `crates/app/src/plugins/web/mod.rs`,
  `host_exe` in `crates/web/tests/common/mod.rs`) return `None` when
  `GHOSTREALM_NO_WEB` is set, so tests skip even when an older bundle exists.
  Toggling the variable reruns the build script and recompiles the app crate
  once.
- `cargo test -p ghostrealm-web` doesn't build the app, so it doesn't refresh
  the bundle: it tests whatever the last app build placed.
- Host unit tests (`browser.rs`): `cargo test -p ghostrealm-web-host
  --features bundle --target-dir target/web-host`. That reuses the nested
  build's CEF; without those flags Cargo builds a second CEF in `target/`.

## Agent default: `GHOSTREALM_NO_WEB=1`

Through skills only, not `.claude/settings.json`:

- build skill: agents export `GHOSTREALM_NO_WEB=1` for every cargo command
  unless the task touches web, including throwaway `CARGO_TARGET_DIR` builds
  and scratchpad workspace copies (the non-macOS compile check).
- sandboxed-build skill: always set it (no CMake or Ninja there, and the CEF
  tests fail anyway).
- web-host skill: web tasks leave it unset; host unit tests run with the flags
  above; `cargo test -p ghostrealm-web` doesn't refresh the bundle.

## `mise run web`, `clean-stale`, `clean-all`

- `mise run web`: removed; `cargo build` does its job. Messages, docs and skills
  that name it switch to `cargo build`: the app's not-found and
  protocol-mismatch messages and their tests, `[web] host` help, the
  `Cargo.toml` comment, `docs/SPEC.md`, the web-host skill, and the test skip
  messages.
- `clean-stale`: `clean_target(target)` also cleans `<target>/web-host` as a
  target dir of its own (its `debug`/`release` locks, the same `Cargo.lock`),
  skipped while a build holds it. This also removes a superseded CEF download
  after a CEF bump.
- `clean-all` emptying an idle agent worktree: `web-host` would be kept whole
  (it has lock files one level down). It becomes a nested target: emptied
  under its own locks, keeping its `CACHEDIR.TAG` and lock files, and kept if
  its locks can't be taken. `last_active` counts its profile dirs.
- Old units, such as the `cef`/`cef-dll-sys` units in the main checkout's
  `target/debug` (~1.8 GB): `cargo test -p ghostrealm-web-host`, `--workspace`
  and rust-analyzer can still build them there, so no rule by package name.
  A new opt-in `--unused-days N` removes a unit whose fingerprint dir and
  `deps/` files were neither modified nor accessed (atime, where recorded) in
  N days. Where atime isn't recorded, an in-use unit older than N days goes
  too and is rebuilt; it costs a compile, never data. One exception: in a
  nested `web-host` target dir, build-script runs (and the compiles they go
  with) of package versions `Cargo.lock` lists are kept whatever their age,
  since without atime the current `cef-dll-sys` run would go and the next
  build would re-download CEF (~250 MB) and fail offline. In the outer target
  dir they age out like other units. `clean-all` doesn't pass
  it; the owner runs `mise run clean-stale -- --unused-days 14` once after
  landing, and whenever wanted.
- New tests (`rustc --test scripts/clean-stale.rs`):
  - superseded units in `web-host/debug` are removed;
  - a held `web-host` lock skips only `web-host`;
  - emptying empties `web-host` and keeps its tag and lock files;
  - a held `web-host` lock during emptying keeps `web-host` whole;
  - `--unused-days`: an old unit goes; one with a recent mtime or atime stays;
    one whose metadata can't be read stays; an old current-lock build-script
    run in `web-host/debug` stays, the same run in `debug` goes.
- Doc comments in `mise.toml` and `clean-stale.rs`, and the build skill, name
  `target/web-host`.

## Non-macOS

The host builds and bundles on Linux too (`bundle.rs` has a Linux path), but
this machine can't run it and the Linux bundle is a plain directory with its
own layout, so the build script skips on every OS but macOS.
`build/web_host.rs`'s placement code is `#[cfg(unix)]` (it reads uids); the
build script compiles to a no-op elsewhere. Compile-check the Linux side per
the build skill's `target_os` swap; Windows can't be compile-checked here.

Removing `mise run web` is a regression on Linux: no command bundles the
host there. The `docs/TODO.md` entry records it, with the two manual commands
(`cargo build -p ghostrealm-web-host --features bundle --bins --target-dir
target/web-host --config 'build.build-dir="target/web-host"'`, then that dir's `debug/ghostrealm-web-bundle`, and copying
the result beside `ghostrealm`). It also records the work: build and place the
Linux bundle from `build.rs` with its own ownership guard.

## The `cef_sandbox` link

It lands in `crates/web-host`, not in the build script: the `sandbox` feature
on the `cef` dependency in `crates/web-host/Cargo.toml` (always on, per
DECISION.md, so no command-line feature), `libcef_sandbox.dylib` copied into
the bundle by `bundle.rs`, and the helper's sandbox entry in `helper.rs`. The
build script's rerun triggers already cover those files. The staged-bundle
completeness check gains `libcef_sandbox.dylib` then.

## Steps (commits)

1. `build(app)`: `build/web_host.rs`: dirs, skip rules, env filter, rerun
   list, stamp, placement with guards; and its tests.
2. `build(app)`: `build.rs` runs the nested build, bundle and placement;
   propagated env triggers; log and progress; `GHOSTREALM_WEB_BUILD` in the
   not-found message.
3. `test(web)`: real-host helpers skip under `GHOSTREALM_NO_WEB`.
4. `build(scripts)`: clean-stale and clean-all cover `target/web-host`.
5. `build(scripts)`: clean-stale `--unused-days`.
6. `build`: remove the `web` mise task; messages, docs and skills name
   `cargo build`; the skill changes under Agent default.
7. `docs`: DECISION.md entry "the app's build script builds and bundles the web
   host". The 2026-10-05 Web surface entry already reads `PARTLY SUPERSEDED
   BY [2026-10-13T20:40+1300@e534aa0] (pop-up handling)`; add the new entry as
   a second reference (building without CEF), since that entry says the app
   builds and tests without CEF. `docs/TODO.md`: the Linux entry; remove the "HIGHEST web priority"
   entry and decisions-pending item 2.

Verification beyond unit tests:

- Fresh worktree `cargo build`: bundle in `target/debug`; `ghostrealm agent`
  `web.new` + `get surface` loads a page. Record first-build and no-op times.
- Second `cargo build -v`: the build script is `Fresh`.
- `cargo test -p ghostrealm` right after: the build script is `Fresh`.
- `cargo clippy`: one rerun; the nested build compiles nothing and the stamp
  skips the swap; the next `cargo clippy` is `Fresh`.
- `touch crates/web-host/src/main.rs; cargo build`: host rebuilt, bundle
  replaced; the next build is `Fresh`.
- `touch crates/web/tests/host.rs; cargo build`: the build script is `Fresh`.
- `rm -rf target/debug/ghostrealm-web-host.app; cargo build`: re-placed.
- `RA_RUSTC_WRAPPER=1 cargo build; cargo build` (a stamp-match run), then
  `rm -rf target/debug/ghostrealm-web-host.app; cargo build`: re-placed.
- `touch crates/web-host/src/bin/bundle.rs; cargo build`: rebundled and
  replaced, though the host and helper executables are unchanged.
- `CARGO_BUILD_BUILD_DIR=<scratch> cargo build`: the nested build doesn't hang
  and writes only under `target/web-host`.
- `cargo build --release`: release bundle in `target/release`.
- `cargo build --target-dir <scratch>`: skipped with the `CARGO_TARGET_DIR`
  hint; `target/debug` untouched.
- `GHOSTREALM_NO_WEB=1 cargo build`: skipped, warning shown, existing bundle
  untouched; unsetting it builds and places.
- First build with `https_proxy=http://127.0.0.1:9` on an empty
  `target/web-host`: fails with the log tail and the hints.
- `cargo test -p ghostrealm`; `rustc --test scripts/clean-stale.rs`;
  `mise run clean-all -- --dry-run`.

## Risks

- rust-analyzer's variable name (`RA_RUSTC_WRAPPER`) is to be confirmed in
  implementation against the installed rust-analyzer. A newer rust-analyzer
  may run its build-script pass as `cargo check --compile-time-deps` and set
  no variable the script can identify. Then its pass runs the full nested
  build in the background: correct (the wrappers are removed), only slow on
  the first run and after host input changes.
- A future Cargo build-dir layout change makes every build a shape skip
  (Dirs). The `OUT_DIR` shape test turns that into a test failure on the
  toolchain that changed it.
- Each host input change recompiles the app crate (Rerun triggers).
- `cargo install --path crates/app` installs no bundle (skipped by 2; out of
  scope).
- Debug and release each keep a CEF copy (~600 MB each).
