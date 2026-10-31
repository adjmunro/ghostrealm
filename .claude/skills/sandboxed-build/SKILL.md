---
name: sandboxed-build
description: Use when the agent command sandbox is on, or builds or PTY, GPU, local-port or CEF tests fail on sandbox restrictions.
---

With the sandbox on, PTY, GPU, local-port and CEF-host tests fail and the build needs workarounds. Flag it for the owner to turn off (`flag-for-owner`, under "Needs you at a desktop"), and meanwhile:

- `GHOSTREALM_NO_WEB=1` always: there is no CMake or Ninja for the web host build, and the CEF tests fail anyway.
- Zig from ziglang.org on `PATH`.
- `CARGO_HOME` and `ZIG_GLOBAL_CACHE_DIR` under `$TMPDIR`.
- `CARGO_NET_GIT_FETCH_WITH_CLI=true` and `GIT_CONFIG_GLOBAL=/dev/null`.
- `GHOSTTY_SOURCE_DIR` set to the extracted pinned Ghostty source, its Zig dependencies `zig fetch`ed, and `xcrun`/`mktemp` in its `src/build/libsystem_override.sh` kept off the per-user temp dir.
