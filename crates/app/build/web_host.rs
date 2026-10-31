//! What the app's build script decides about the CEF web host: when to skip
//! it, where the nested build and the bundle go, which inputs rerun it, and
//! how the bundle replaces the one beside the app binary. `build.rs` runs the
//! commands; `tests/web_host_build.rs` includes this file to test it.
//!
//! Placement replaces a bundle this build may not have made, so every error,
//! missing, empty, truncated or unreadable input keeps the existing one.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// The host executable, and the name of the bundle around it.
pub const HOST: &str = "ghostrealm-web-host";
pub const HELPER: &str = "ghostrealm-web-host-helper";
pub const BUNDLER: &str = "ghostrealm-web-bundle";
/// `ghostrealm_web::temp::BUNDLE_ID`, which the bundler writes into the
/// bundle's `Info.plist` (a test checks they agree).
pub const BUNDLE_ID: &str = "dev.ghostrealm.web-host";
/// Bumped whenever [`complete`]'s rules or the placement layout change, so a
/// bundle made under the old rules no longer matches its stamp.
pub const BUNDLE_RULES: u32 = 1;
pub const APP: &str = "ghostrealm-web-host.app";
/// Where the bundle being replaced waits until the new one is in place.
pub const OLD: &str = ".ghostrealm-web-host.app.old";
/// What the bundle was made from, inside it.
pub const STAMP: &str = "Contents/Resources/ghostrealm-stamp";
/// The packages in `ghostrealm-web-host`'s path-dependency closure.
pub const WATCHED_PACKAGES: [&str; 3] = ["crates/web-host", "crates/web", "crates/process"];
/// The helper apps CEF's bundler makes, as `<HOST> <helper>.app`.
pub const HELPERS: [&str; 5] = ["Helper (GPU)", "Helper (Renderer)", "Helper (Plugin)", "Helper (Alerts)", "Helper"];
const FRAMEWORK: &str = "Contents/Frameworks/Chromium Embedded Framework.framework/Chromium Embedded Framework";
/// How far before the build script's start the placed executable's mtime is
/// put: Cargo reruns the script when a `rerun-if-changed` file is newer than
/// the run's start, and some file systems keep whole seconds.
pub const SETTLE: Duration = Duration::from_secs(5);

/// Variables the skip rules and the dirs read: a change reruns the script.
pub const TRACKED_ENV: [&str; 5] =
    ["GHOSTREALM_NO_WEB", "RA_RUSTC_WRAPPER", "CARGO_TARGET_DIR", "CARGO_BUILD_TARGET_DIR", "CARGO_BUILD_BUILD_DIR"];

/// Whether a `GHOSTREALM_NO_WEB` value asks to skip the web host: set, and
/// neither empty nor `0`.
pub fn no_web(value: Option<&str>) -> bool {
    value.is_some_and(|v| !v.is_empty() && v != "0")
}

/// Why the web host isn't built, from the build script's environment alone
/// (`var` reads one variable), or `None`.
pub fn env_skip(var: impl Fn(&str) -> Option<String>) -> Option<String> {
    if no_web(var("GHOSTREALM_NO_WEB").as_deref()) {
        return Some("GHOSTREALM_NO_WEB is set".into());
    }
    if var("RA_RUSTC_WRAPPER").is_some() {
        return Some("rust-analyzer's build-script pass".into());
    }
    let os = var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if os != "macos" {
        return Some(format!("the web host is bundled only for macOS targets, not {os}"));
    }
    let (target, host) = (var("TARGET").unwrap_or_default(), var("HOST").unwrap_or_default());
    if target != host {
        return Some(format!("cross-compiling for {target} on {host}; the bundler runs on the build machine"));
    }
    None
}

/// Where the app's build puts things, and where the nested host build goes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dirs {
    /// Cargo's target dir (canonical).
    pub target: PathBuf,
    /// Where the app binary goes: `<target>/[<triple>/]<profile dir>`.
    pub artifact: PathBuf,
    /// The nested build's target (and build) dir: `<target>/web-host`.
    pub nested: PathBuf,
    /// Set when the outer build names a target triple.
    pub triple: Option<String>,
    /// `debug`, `release`, or a custom profile's name.
    pub profile_dir: String,
}

impl Dirs {
    /// The `--profile` that writes to [`profile_dir`](Self::profile_dir).
    pub fn profile(&self) -> &str {
        if self.profile_dir == "debug" { "dev" } else { &self.profile_dir }
    }

    /// The nested build's profile dir, where its executables, the staged
    /// bundle and the build log go.
    pub fn nested_profile(&self) -> PathBuf {
        let mut p = self.nested.clone();
        if let Some(t) = &self.triple {
            p.push(t);
        }
        p.join(&self.profile_dir)
    }
}

/// Why the dirs can't be worked out.
#[derive(Debug, PartialEq, Eq)]
pub enum DirsError {
    /// `OUT_DIR` isn't where Cargo's metadata says the build goes: skip.
    Layout(String),
    /// Something that should exist doesn't: fail.
    Fail(String),
}

/// The hint a layout skip gives.
pub const LAYOUT_HINT: &str = "the build script can't see a command-line `--target-dir` or \
    `--config build.target-dir`, and resolves a relative `CARGO_TARGET_DIR` from the workspace \
    root; set `CARGO_TARGET_DIR` to an absolute path instead";

/// The dirs, from the build script's `OUT_DIR`, `cargo metadata`'s
/// `target_directory` and `build_directory` (absent before Cargo had a
/// separate build dir: then the target dir), and `$TARGET`.
///
/// `OUT_DIR` must be `<profile>/build/ghostrealm-<hash>/out` or
/// `<triple>/<profile>/build/ghostrealm-<hash>/out` under the build dir:
/// otherwise this build's target dir isn't the one metadata names (a
/// command-line `--target-dir`, `cargo install`), whose lock this build
/// doesn't hold.
pub fn dirs(out_dir: &Path, target_dir: &Path, build_dir: Option<&Path>, triple: &str) -> Result<Dirs, DirsError> {
    let canon = |p: &Path| fs::canonicalize(p).map_err(|e| DirsError::Fail(format!("{}: {e}", p.display())));
    let out = canon(out_dir)?;
    let target = canon(target_dir)?;
    let build = match build_dir {
        Some(b) => canon(b)?,
        None => target.clone(),
    };
    let layout = |why: String| DirsError::Layout(format!("{why}; {LAYOUT_HINT}"));
    let rel = out.strip_prefix(&build).map_err(|_| {
        layout(format!("OUT_DIR {} is not under the build dir {}", out.display(), build.display()))
    })?;
    let parts: Vec<String> = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
    let unit = |p: &str| {
        p.strip_prefix("ghostrealm-").is_some_and(|h| !h.is_empty() && h.bytes().all(|b| b.is_ascii_hexdigit()))
    };
    let (t, profile_dir) = match parts.as_slice() {
        [p, b, u, o] if b == "build" && unit(u) && o == "out" => (None, p.clone()),
        [t, p, b, u, o] if t == triple && b == "build" && unit(u) && o == "out" => (Some(t.clone()), p.clone()),
        _ => return Err(layout(format!("OUT_DIR {} has an unknown layout under {}", out.display(), build.display()))),
    };
    let mut artifact = target.clone();
    if let Some(t) = &t {
        artifact.push(t);
    }
    artifact.push(&profile_dir);
    if !fs::symlink_metadata(&artifact).is_ok_and(|m| m.is_dir()) {
        return Err(DirsError::Fail(format!("{}: not a directory", artifact.display())));
    }
    Ok(Dirs { nested: target.join("web-host"), target, artifact, triple: t, profile_dir })
}

/// `s` as a TOML basic string.
fn toml_string(s: &str) -> String {
    let mut out = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The nested `cargo` arguments. `--target-dir` alone doesn't override a
/// `build.build-dir` inherited from `CARGO_BUILD_BUILD_DIR` or config, and a
/// nested build in the outer build dir waits forever on its lock.
pub fn nested_args(dirs: &Dirs) -> Vec<OsString> {
    let nested = dirs.nested.to_string_lossy();
    let mut args: Vec<OsString> =
        ["build", "--locked", "-p", HOST, "--features", "bundle", "--bins", "--target-dir"].map(OsString::from).into();
    args.push(dirs.nested.clone().into());
    args.push("--config".into());
    args.push(format!("build.build-dir={}", toml_string(&nested)).into());
    if let Some(t) = &dirs.triple {
        args.push("--target".into());
        args.push(t.into());
    }
    for a in ["--profile", dirs.profile(), "--message-format", "json-render-diagnostics", "-vv"] {
        args.push(a.into());
    }
    args
}

/// Whether the nested build and the bundler inherit variable `name`. Cargo
/// sets the dropped ones for this build script (the nested cargo sets its
/// own), and the wrappers belong to the outer command (clippy,
/// rust-analyzer). Config variables (`CARGO_*`, `RUSTFLAGS`) and the
/// jobserver (`CARGO_MAKEFLAGS`) stay, so the nested build computes the same
/// flags and shares the outer job slots.
pub fn keep_env(name: &str) -> bool {
    const DROP: [&str; 13] = [
        "OUT_DIR",
        "TARGET",
        "HOST",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "NUM_JOBS",
        "RUSTC_LINKER",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_ENCODED_RUSTFLAGS",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ];
    const DROP_PREFIX: [&str; 5] = ["CARGO_PKG_", "CARGO_MANIFEST_", "CARGO_CFG_", "CARGO_FEATURE_", "DEP_"];
    !DROP.contains(&name) && !DROP_PREFIX.iter().any(|p| name.starts_with(p))
}

/// The files and dirs whose change reruns the script: each watched
/// package's `src`, `Cargo.toml` and `build.rs`, and the workspace's lock
/// file, manifest (profiles), Cargo config and `mise.toml` (CMake and Ninja
/// pins). Only those that exist: Cargo reruns a script on every build while
/// a `rerun-if-changed` path is missing.
pub fn rerun_paths(root: &Path) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for pkg in WATCHED_PACKAGES {
        for f in ["src", "Cargo.toml", "build.rs"] {
            paths.push(root.join(pkg).join(f));
        }
    }
    for f in ["Cargo.lock", "Cargo.toml", ".cargo/config.toml", "mise.toml"] {
        paths.push(root.join(f));
    }
    paths.retain(|p| fs::symlink_metadata(p).is_ok());
    paths
}

/// The variables a nested build script's `output` file says it reads, in
/// either spelling, less those [`keep_env`] drops (never passed on).
pub fn env_triggers(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|l| {
            l.strip_prefix("cargo::rerun-if-env-changed=").or_else(|| l.strip_prefix("cargo:rerun-if-env-changed="))
        })
        .map(str::trim)
        .filter(|n| !n.is_empty() && keep_env(n))
        .map(str::to_string)
        .collect()
}

/// What the nested build's JSON messages report.
#[derive(Debug, Default)]
pub struct Built {
    pub host: Option<PathBuf>,
    pub helper: Option<PathBuf>,
    pub bundler: Option<PathBuf>,
    /// Each build script's `OUT_DIR`.
    pub out_dirs: Vec<PathBuf>,
}

impl Built {
    /// Take in one line of `--message-format json` output (other lines are
    /// ignored).
    pub fn read(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(line) else { return };
        match msg["reason"].as_str() {
            Some("compiler-artifact") => {
                let Some(exe) = msg["executable"].as_str() else { return };
                let slot = match msg["target"]["name"].as_str() {
                    Some(HOST) => &mut self.host,
                    Some(HELPER) => &mut self.helper,
                    Some(BUNDLER) => &mut self.bundler,
                    _ => return,
                };
                *slot = Some(PathBuf::from(exe));
            }
            Some("build-script-executed") => {
                if let Some(d) = msg["out_dir"].as_str() {
                    self.out_dirs.push(PathBuf::from(d));
                }
            }
            _ => {}
        }
    }
}

/// `target_directory` and `build_directory` from `cargo metadata` JSON.
pub fn metadata_dirs(json: &str) -> Result<(PathBuf, Option<PathBuf>), String> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| format!("cargo metadata: {e}"))?;
    let target = v["target_directory"].as_str().ok_or("cargo metadata: no target_directory")?;
    Ok((PathBuf::from(target), v["build_directory"].as_str().map(PathBuf::from)))
}

/// What a bundle is made from: [`BUNDLE_RULES`], and the mtime (ns) and size
/// of the host, helper and bundler executables.
pub fn stamp(host: &Path, helper: &Path, bundler: &Path) -> io::Result<String> {
    let mut s = format!("rules {BUNDLE_RULES}\n");
    for (name, p) in [("host", host), ("helper", helper), ("bundler", bundler)] {
        let m = fs::metadata(p)?;
        let ns = m.modified()?.duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
        s.push_str(&format!("{name} {ns} {}\n", m.len()));
    }
    Ok(s)
}

/// Whether `path` doesn't exist. Any error but "not found" is an error.
#[cfg_attr(not(unix), allow(dead_code))]
fn missing(path: &Path) -> Result<bool, String> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// Whether `app` is a web host bundle this user's builds made: a real
/// directory owned by `uid` whose `Info.plist` names [`BUNDLE_ID`].
#[cfg(unix)]
pub fn ours(app: &Path, uid: u32) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let at = app.display();
    let m = fs::symlink_metadata(app).map_err(|e| format!("{at}: {e}"))?;
    if !m.is_dir() {
        return Err(format!("{at}: not a directory"));
    }
    if m.uid() != uid {
        return Err(format!("{at}: owned by uid {}, not {uid}", m.uid()));
    }
    let plist = app.join("Contents/Info.plist");
    let out = std::process::Command::new("/usr/bin/plutil")
        .args(["-extract", "CFBundleIdentifier", "raw", "-o", "-"])
        .arg(&plist)
        .output()
        .map_err(|e| format!("{}: plutil: {e}", plist.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(format!("{}: no bundle id ({})", plist.display(), err.trim()));
    }
    let id = out.stdout.strip_suffix(b"\n").unwrap_or(&out.stdout);
    if id != BUNDLE_ID.as_bytes() {
        return Err(format!("{}: bundle id {:?}, not {BUNDLE_ID}", plist.display(), String::from_utf8_lossy(id)));
    }
    Ok(())
}

/// The files a runnable bundle can't do without, relative to it.
pub fn required_files() -> Vec<String> {
    let mut files = vec![format!("Contents/MacOS/{HOST}"), FRAMEWORK.to_string(), STAMP.to_string()];
    for h in HELPERS {
        files.push(format!("Contents/Frameworks/{HOST} {h}.app/Contents/MacOS/{HOST} {h}"));
    }
    files
}

/// Whether `app` holds every [`required_files`] entry as a non-empty file.
pub fn complete(app: &Path) -> Result<(), String> {
    for f in required_files() {
        let p = app.join(&f);
        if !fs::symlink_metadata(&p).is_ok_and(|m| m.is_file() && m.len() > 0) {
            return Err(format!("{}: missing or empty", p.display()));
        }
    }
    Ok(())
}

/// Whether the bundle at `dest` is ours and was made from `stamp`.
#[cfg(unix)]
pub fn current(dest: &Path, stamp: &str, uid: u32) -> bool {
    fs::read_to_string(dest.join(STAMP)).is_ok_and(|s| s == stamp) && ours(dest, uid).is_ok()
}

/// Remove a bundle, its `Info.plist` last: a removal that fails part way
/// leaves a bundle the next run still recognises as ours.
#[cfg_attr(not(unix), allow(dead_code))]
fn remove_bundle(app: &Path) -> io::Result<()> {
    let contents = app.join("Contents");
    if let Ok(entries) = fs::read_dir(&contents) {
        for e in entries {
            let e = e?;
            if e.file_name() == "Info.plist" {
                continue;
            }
            if e.file_type()?.is_dir() {
                fs::remove_dir_all(e.path())?;
            } else {
                fs::remove_file(e.path())?;
            }
        }
    }
    fs::remove_dir_all(app)
}

/// Remove a leftover `<artifact>/OLD` if it is ours. Ok(false): there was
/// none.
#[cfg(unix)]
fn remove_old(artifact: &Path, uid: u32) -> Result<bool, String> {
    let old = artifact.join(OLD);
    if missing(&old)? {
        return Ok(false);
    }
    ours(&old, uid).map_err(|e| format!("{e}; left by an earlier build, but not recognised: remove it by hand"))?;
    remove_bundle(&old).map_err(|e| format!("{}: {e}", old.display()))?;
    Ok(true)
}

/// Move the bundle at `staged` to `<artifact>/APP`, replacing the one there
/// only if it is ours. On any error the existing bundle stays (restored from
/// `OLD` if it had been moved). Returns warnings that don't fail the step.
#[cfg(unix)]
pub fn place(staged: &Path, artifact: &Path, uid: u32) -> Result<Vec<String>, String> {
    ours(staged, uid)?;
    complete(staged)?;
    let dest = artifact.join(APP);
    let old = artifact.join(OLD);
    let rename = |from: &Path, to: &Path| {
        fs::rename(from, to).map_err(|e| format!("rename {} to {}: {e}", from.display(), to.display()))
    };
    // A killed run may have left the bundle at `OLD`.
    if missing(&dest)? && !missing(&old)? && ours(&old, uid).is_ok() {
        rename(&old, &dest)?;
    }
    remove_old(artifact, uid)?;
    let had = !missing(&dest)?;
    if had {
        ours(&dest, uid).map_err(|e| format!("{e}; not replaced"))?;
        rename(&dest, &old)?;
    }
    if let Err(e) = rename(staged, &dest) {
        if had {
            if let Err(r) = rename(&old, &dest) {
                return Err(format!("{e}; and restoring it: {r}"));
            }
        }
        return Err(e);
    }
    let mut warnings = Vec::new();
    if had {
        if let Err(e) = remove_bundle(&old) {
            warnings.push(format!("{}: {e}; removed by the next build", old.display()));
        }
    }
    Ok(warnings)
}

/// Put `exe`'s mtime at most `start - SETTLE`, so the `rerun-if-changed` on
/// it stays quiet until it is changed or removed. It is the bundle's own
/// copy (a separate file from the nested build's executable).
pub fn settle(exe: &Path, start: SystemTime) -> io::Result<()> {
    let at = start.checked_sub(SETTLE).unwrap_or(SystemTime::UNIX_EPOCH);
    if fs::symlink_metadata(exe)?.modified()? > at {
        fs::File::open(exe)?.set_modified(at)?;
    }
    Ok(())
}

/// What [`install`] did.
#[derive(Debug, Default)]
pub struct Installed {
    /// A new bundle went into place (else the one there was current).
    pub placed: bool,
    /// The placed host executable, to emit `rerun-if-changed` on; `None`
    /// when its mtime couldn't be settled (a warning says so).
    pub trigger: Option<PathBuf>,
    pub warnings: Vec<String>,
}

/// Bring `<artifact>/APP` up to date with `stamp`: when the bundle there is
/// current, only a leftover `OLD` of ours goes; otherwise `bundle` makes a
/// staged bundle, which gets the stamp and is [`place`]d.
#[cfg(unix)]
pub fn install(
    artifact: &Path,
    stamp: &str,
    uid: u32,
    start: SystemTime,
    bundle: impl FnOnce() -> Result<PathBuf, String>,
) -> Result<Installed, String> {
    let dest = artifact.join(APP);
    let mut done = Installed::default();
    if current(&dest, stamp, uid) {
        if let Err(e) = remove_old(artifact, uid) {
            done.warnings.push(e);
        }
    } else {
        let staged = bundle()?;
        let stamp_at = staged.join(STAMP);
        if let Some(dir) = stamp_at.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        fs::write(&stamp_at, stamp).map_err(|e| format!("{}: {e}", stamp_at.display()))?;
        done.warnings = place(&staged, artifact, uid)?;
        done.placed = true;
    }
    let exe = dest.join("Contents/MacOS").join(HOST);
    match settle(&exe, start) {
        Ok(()) => done.trigger = Some(exe),
        Err(e) => done.warnings.push(format!(
            "{}: can't set its mtime ({e}); a removed bundle is rebuilt only when a host input changes",
            exe.display()
        )),
    }
    Ok(done)
}
