---
name: web-host
description: Use when building, bundling, testing or driving the CEF web host (crates/web-host).
---

- The web host is not a default workspace member: plain `cargo build/test` skip it. Build and bundle it with `mise run web` (CMake and Ninja come from `mise.toml`).
- Toggling its `bundle` feature rebuilds `cef`, so always build it via `mise run web`.
- Its integration test (`cargo test -p ghostrealm-web --test host`) skips unless the bundle exists.
- The host runs headless, so web behaviour is verifiable without a GUI: drive it with `ghostrealm agent` (`web.new`, `web.eval`, `get surface`) or the host test.
