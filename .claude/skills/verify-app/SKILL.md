---
name: verify-app
description: Use when checking that ghostrealm works, renders or performs, or when judging speed or startup time.
---

- The sandbox has no display: a launched `ghostrealm` window never fires winit's `resumed`, so it neither draws nor writes its config; it idles without error. "Still alive after 2s" proves only that startup didn't crash, not that it renders.
- Verify headlessly: `cargo test` (core logic, backend PTY/VT, agent protocol, inbox, config) and the offscreen wgpu render test. Drive behaviour through `ghostrealm agent` (JSON on stdin/stdout) or `ghostrealm dump <cmd>` (grid as text). Visual confirmation needs a human on a desktop session.
- Judge performance (scroll, typing, rendering) only from `--release` builds. The dev profile leaves font shaping (cosmic-text/swash) unoptimised, so a debug build is slow on its own.
- On macOS, a test that spawns the debug `ghostrealm` binary waits ~2s before its `main` runs (process load). Time from `main` before blaming a spawned server or CLI path.
