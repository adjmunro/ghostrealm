//! Builds the CEF web host in its own target dir (`<target>/web-host`),
//! bundles it there, and moves the bundle beside the app binary, where the
//! app looks for it. `GHOSTREALM_NO_WEB=1` skips it. The rules are in
//! `build/web_host.rs`. It also prunes superseded units from `target/` once
//! the build is done (`build/prune.rs`).

#[path = "build/prune.rs"]
mod prune;
#[path = "build/watch.rs"]
mod watch;
#[path = "build/web_host.rs"]
mod web_host;

use std::env;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

fn main() {
    let start = SystemTime::now();
    for v in web_host::TRACKED_ENV {
        println!("cargo::rerun-if-env-changed={v}");
    }
    let target = env::var("TARGET").unwrap_or_default();
    // The OUT_DIR shape test needs the triple this build is for.
    println!("cargo::rustc-env=GHOSTREALM_TARGET={target}");
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let root = std::fs::canonicalize(manifest.join("../..")).unwrap_or_else(|e| fail(&format!("workspace root: {e}")));
    for p in web_host::rerun_paths(&root) {
        println!("cargo::rerun-if-changed={}", p.display());
    }
    #[cfg(unix)]
    {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
        let absent = out_dir.join("web-host-watch");
        let at = start.checked_sub(web_host::SETTLE).unwrap_or(SystemTime::UNIX_EPOCH);
        match watch::watch_absent(&absent, &web_host::absent_build_scripts(&root), at) {
            Ok(()) => println!("cargo::rerun-if-changed={}", absent.display()),
            Err(e) => println!(
                "cargo::warning={}: {e}; a new build.rs in a web host package won't rerun this script",
                absent.display()
            ),
        }
    }
    #[cfg(unix)]
    prune_after_build(&root);
    if let Some(why) = web_host::env_skip(|k| env::var(k).ok()) {
        return skip(&why);
    }
    #[cfg(unix)]
    host::build(start, &root, &target);
    #[cfg(not(unix))]
    let _ = (start, root);
}

/// Starts clean-stale detached, if due; it waits for this build to finish.
#[cfg(unix)]
fn prune_after_build(root: &Path) {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR"));
    let Some(target) = prune::own_target(root, &out_dir, env::var("CARGO_TARGET_DIR").ok().as_deref()) else {
        return;
    };
    if !prune::due(env::var(prune::OPT_OUT).ok().as_deref(), &target.join(prune::STAMP), SystemTime::now()) {
        return;
    }
    let bin = target.join(prune::BIN);
    // Not inherited pipes: cargo reads this script's output until they close.
    let Ok(out) = std::fs::File::create(bin.with_extension("log")) else { return };
    let Ok(err) = out.try_clone() else { return };
    // Builds the pruner when the script is newer, then waits for the locks.
    let script = r#"cd "$1" && { [ "$2" -nt scripts/clean-stale.rs ] || "$3" -O --edition 2021 scripts/clean-stale.rs -o "$2"; } && exec "$2" --wait "$5" --target-dir "$4" --lockfile Cargo.lock"#;
    let spawned = Command::new("sh")
        .args(["-c", script, "sh"])
        .arg(root)
        .arg(&bin)
        .arg(env::var_os("RUSTC").unwrap_or_else(|| "rustc".into()))
        .arg(&target)
        .arg(prune::WAIT)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn();
    if let Err(e) = spawned {
        println!("cargo::warning=target/ not pruned: {e}");
    }
}

fn skip(why: &str) {
    println!("cargo::warning=web host not built: {why}");
    println!("cargo::rustc-env=GHOSTREALM_WEB_BUILD={why}");
}

/// Fail the build script with `msg`.
fn fail(msg: &str) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(1)
}

/// Fail after the nested build or the bundler failed: the end of `log`, its
/// path, and what usually fixes it.
#[cfg_attr(not(unix), allow(dead_code))]
fn fail_with_log(msg: &str, log: &Path) -> ! {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    // Without the JSON messages and `-vv`'s command lines.
    let lines: Vec<&str> =
        text.lines().filter(|l| !l.starts_with('{') && !l.trim_start().starts_with("Running `")).collect();
    eprintln!("{}", lines[lines.len().saturating_sub(40)..].join("\n"));
    eprintln!("\nerror: {msg}; full log: {}", log.display());
    let missing: Vec<&str> = ["cmake", "ninja"].into_iter().filter(|t| !on_path(t)).collect();
    if !missing.is_empty() {
        eprintln!("{} not on PATH.", missing.join(" and "));
    }
    eprintln!(
        "Run cargo from the repo root so mise supplies CMake and Ninja (mise.toml), \
         or set GHOSTREALM_NO_WEB=1 to build without the web host."
    );
    std::process::exit(1)
}

#[cfg_attr(not(unix), allow(dead_code))]
fn on_path(tool: &str) -> bool {
    env::var_os("PATH").is_some_and(|p| env::split_paths(&p).any(|d| d.join(tool).is_file()))
}

#[cfg(unix)]
mod host {
    use super::{fail, fail_with_log, skip, web_host};
    use std::ffi::OsString;
    use std::fs::{self, File};
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;
    use web_host::{Built, DirsError};

    /// Progress for a person watching: cargo shows nothing from a build
    /// script until it ends. Only to a terminal that isn't an agent's or
    /// CI's, and never while rust-analyzer runs the script.
    struct Tty(Option<File>);

    impl Tty {
        fn open() -> Tty {
            let set = |k: &str| std::env::var_os(k).is_some();
            let term = std::env::var("TERM").unwrap_or_default();
            if set("CLAUDECODE") || set("CI") || set("RA_RUSTC_WRAPPER") || term.is_empty() || term == "dumb" {
                return Tty(None);
            }
            Tty(fs::OpenOptions::new().write(true).open("/dev/tty").ok())
        }

        fn line(&mut self, s: &str) {
            if let Some(f) = &mut self.0 {
                // Over cargo's progress bar, which it redraws after.
                let _ = write!(f, "\r\x1b[2K{s}\n");
            }
        }
    }

    pub fn build(start: SystemTime, root: &Path, triple: &str) {
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
        let env: Vec<(OsString, OsString)> =
            std::env::vars_os().filter(|(k, _)| k.to_str().is_some_and(web_host::keep_env)).collect();
        let command = |program: &OsString| {
            let mut c = Command::new(program);
            c.current_dir(root).env_clear().envs(env.iter().cloned()).stdin(Stdio::null());
            c
        };

        let meta = command(&cargo)
            .args(["metadata", "--no-deps", "--format-version", "1"])
            .output()
            .unwrap_or_else(|e| fail(&format!("cargo metadata: {e}")));
        if !meta.status.success() {
            fail(&format!("cargo metadata failed:\n{}", String::from_utf8_lossy(&meta.stderr)));
        }
        let (target_dir, build_dir) =
            web_host::metadata_dirs(&String::from_utf8_lossy(&meta.stdout)).unwrap_or_else(|e| fail(&e));
        let dirs = match web_host::dirs(&out_dir, &target_dir, build_dir.as_deref(), triple) {
            Ok(d) => d,
            Err(DirsError::Layout(why)) => return skip(&why),
            Err(DirsError::Fail(e)) => fail(&e),
        };
        if let Err(why) = web_host::held_by_this_build(&dirs.artifact) {
            return skip(&why);
        }

        let profile = dirs.nested_profile();
        fs::create_dir_all(&profile).unwrap_or_else(|e| fail(&format!("{}: {e}", profile.display())));
        let log_path = profile.join("ghostrealm-build.log");
        let log = File::create(&log_path).unwrap_or_else(|e| fail(&format!("{}: {e}", log_path.display())));
        let log = Arc::new(Mutex::new(log));
        let mut tty = Tty::open();
        tty.line(&format!(
            "    Building web host (the first build downloads CEF, ~250 MB); log: {}",
            log_path.display()
        ));

        let mut child = command(&cargo)
            .args(web_host::nested_args(&dirs))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap_or_else(|e| fail(&format!("cargo build of the web host: {e}")));
        let stderr = child.stderr.take().expect("piped");
        let err_log = Arc::clone(&log);
        let err_thread = std::thread::spawn(move || {
            let mut cef = false;
            let mut warnings = 0;
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                let _ = writeln!(err_log.lock().unwrap(), "{line}");
                warnings += web_host::warning_count(&line).unwrap_or(0);
                let t = line.trim_start();
                if t.starts_with("Compiling ") {
                    tty.line(&line);
                } else if !cef && t.contains("/cef-dll-sys-") && t.trim_end().ends_with("build-script-build`") {
                    cef = true;
                    tty.line("     Running cef-dll-sys: fetches CEF, then builds its C++ wrapper");
                }
            }
            (tty, warnings)
        });
        let mut built = Built::default();
        for line in BufReader::new(child.stdout.take().expect("piped")).lines().map_while(Result::ok) {
            let _ = writeln!(log.lock().unwrap(), "{line}");
            built.read(&line);
        }
        let status = child.wait().unwrap_or_else(|e| fail(&format!("cargo build of the web host: {e}")));
        let (mut tty, warnings) = err_thread.join().unwrap_or((Tty(None), 0));
        if !status.success() {
            fail_with_log("the web host build failed", &log_path);
        }
        // Cargo shows a nested build's diagnostics nowhere but the log.
        if warnings > 0 {
            println!("cargo::warning=the web host build had {warnings} compiler warnings: {}", log_path.display());
        }
        let exe = |p: Option<PathBuf>, name: &str| {
            p.unwrap_or_else(|| fail(&format!("the web host build reported no `{name}` executable")))
        };
        let host = exe(built.host, web_host::HOST);
        let helper = exe(built.helper, web_host::HELPER);
        let bundler = exe(built.bundler, web_host::BUNDLER);

        // The nested build scripts' inputs are this build's too.
        let mut vars: Vec<String> = Vec::new();
        for d in &built.out_dirs {
            let output = fs::read_to_string(d.with_file_name("output")).unwrap_or_default();
            vars.extend(web_host::env_triggers(&output));
        }
        vars.sort();
        vars.dedup();
        for v in vars {
            println!("cargo::rerun-if-env-changed={v}");
        }

        let stamp = web_host::stamp(&host, &helper, &bundler)
            .unwrap_or_else(|e| fail(&format!("reading the web host executables: {e}")));
        let uid = {
            use std::os::unix::fs::MetadataExt;
            fs::metadata(&out_dir).unwrap_or_else(|e| fail(&format!("{}: {e}", out_dir.display()))).uid()
        };
        let dir = bundler.parent().map(Path::to_path_buf).unwrap_or_default();
        let bundle = || {
            // The bundler bundles the executables beside itself.
            if host.parent() != Some(dir.as_path()) || helper.parent() != Some(dir.as_path()) {
                return Err(format!("the host, helper and bundler aren't in one dir: {}", dir.display()));
            }
            tty.line("    Bundling web host");
            let out = command(&bundler.clone().into_os_string())
                .current_dir(&dir)
                .output()
                .map_err(|e| format!("{}: {e}", bundler.display()))?;
            let mut f = log.lock().unwrap();
            let _ = f.write_all(&out.stdout);
            let _ = f.write_all(&out.stderr);
            drop(f);
            if !out.status.success() {
                fail_with_log("the web host bundler failed", &log_path);
            }
            Ok(dir.join(web_host::APP))
        };
        let done = web_host::install(&dirs.artifact, &stamp, uid, start, bundle).unwrap_or_else(|e| fail(&e));
        for w in &done.warnings {
            println!("cargo::warning={w}");
        }
        if let Some(exe) = done.trigger {
            println!("cargo::rerun-if-changed={}", exe.display());
        }
        let took = SystemTime::now().duration_since(start).map_or(0.0, |d| d.as_secs_f64());
        let what = if done.placed { "built and placed" } else { "up to date" };
        tty.line(&format!("    Finished web host {what} in {took:.1} s: {}", dirs.artifact.join(web_host::APP).display()));
    }
}
