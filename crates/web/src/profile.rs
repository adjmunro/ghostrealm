//! Where a web host keeps its profile: CEF's `root_cache_path`, which holds
//! the profile's cookies, cache and local storage in `Default/`.

use std::path::PathBuf;

/// Overrides the profile root for a host spawned with it in its environment.
pub const CACHE_ENV: &str = "GHOSTREALM_WEB_CACHE";

/// The profile root: `$GHOSTREALM_WEB_CACHE`, else the person's
/// ([`base`]`/web`).
pub fn root() -> PathBuf {
    std::env::var_os(CACHE_ENV).map(PathBuf::from).unwrap_or_else(|| base().join("web"))
}

/// The profile root of the headless agent channel (`ghostrealm agent
/// --headless`, or with no window): apart from the person's, so an agent
/// run outside a window never holds the person's sessions.
pub fn headless_agent_root() -> PathBuf {
    base().join("web-agent")
}

/// The platform cache dir's `ghostrealm`.
fn base() -> PathBuf {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let cache = if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache"))
    };
    cache.join("ghostrealm")
}
