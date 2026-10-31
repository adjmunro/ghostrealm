---
name: web-host
description: Use when building, bundling, testing or driving the CEF web host (crates/web-host).
---

- The app's build script (`crates/app/build.rs`) builds the host in `target/web-host`, bundles it there and places it beside the app binary, so plain `cargo build`, `cargo run` and `cargo test -p ghostrealm` produce it, cached until a host input changes. CMake and Ninja come from `mise.toml`: run cargo from the repo root. Web tasks leave `GHOSTREALM_NO_WEB` unset.
- A failed host build fails the app build; the log is `target/web-host/<profile>/ghostrealm-build.log`.
- The host is not a default workspace member. Its unit tests (`browser.rs`): `cargo test -p ghostrealm-web-host --features bundle --target-dir target/web-host`, which reuses the nested build's CEF; without those flags Cargo builds a second CEF in `target/`.
- `cargo test -p ghostrealm-web` doesn't build the app, so it doesn't refresh the bundle: it tests whatever the last app build placed. Run `cargo build` first after changing the host.
- The real-host tests (`crates/web/tests`, the app's `real_host`) skip unless the bundle exists, and when `GHOSTREALM_NO_WEB` is set.
- The host runs headless, so web behaviour is verifiable without a GUI: drive it with `ghostrealm agent` (`web.new`, `web.eval`, `get surface`) or the host test.
