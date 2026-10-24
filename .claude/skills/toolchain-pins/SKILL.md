---
name: toolchain-pins
description: Use when changing the Zig version, the pinned Ghostty, or the libghostty-vt dependency.
---

- Don't bump Zig blindly: its major.minor must equal the pinned Ghostty's `build.zig.zon` `minimum_zig_version`, and it must be new enough to link this host's macOS SDK. See `docs/libghostty-findings.md` → "Build toolchain".
- `libghostty-vt` comes from git (pinned `rev`), not crates.io: the published crate is stale.
