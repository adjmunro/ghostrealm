//! Removes build artifacts under `target/{debug,release}` that a newer build of
//! the same unit superseded. Cargo never removes them: every change of
//! toolchain, dependency version, feature set, or profile leaves the old
//! hashed copies in `deps/`, `build/`, `.fingerprint/` and `incremental/`.
//!
//! A unit (one `.fingerprint/<package>-<hash>` directory) is identified by what
//! its fingerprint records: target kind (`lib`, `test-bin`, `build-script`,
//! ...), features, target hash, rustflags, and the source file it was
//! compiled from (which tells package versions apart). Of the units
//! with the same identity the `--keep` most recently built (default 3) stay and
//! the rest are removed, with their `deps/` files (`<name>-<hash>*`) and
//! `build/<package>-<hash>/`. Profile hashes are not part of the identity, so a
//! profile change supersedes the old copies. Several stay because
//! `cargo build`, `cargo test --workspace`, `cargo clippy` and `-p` builds
//! resolve features differently and so produce sibling copies of the same
//! unit that are all in use; the number kept bounds the stale ones. A unit
//! reused by the latest build but older than `--keep` newer siblings is
//! removed too; it is rebuilt on the next build. A unit built from a registry
//! package version that `Cargo.lock` no longer lists is removed outright.
//! Units whose fingerprint can't be read are left alone.
//!
//! A build-script run records neither features nor source, so it is not
//! grouped: it goes with the build-script compile it ran (its `out/` can hold
//! a large download, such as CEF). A run whose compile is already gone is
//! removed once another run of the same package has a compile that stays.
//!
//! `incremental/` has no such record, so per crate the newest directories are
//! kept, as many as the crate has units left (at least 4). It is a cache; a
//! lost directory only costs one slower compile.
//!
//! Only `target/` is touched; `~/.cargo` is never read or written. A target
//! dir is skipped while a build holds Cargo's lock on it (`.cargo-lock`,
//! `.cargo-build-lock`), and that lock is held while cleaning, so a build that
//! starts meanwhile waits. Cargo releases the lock before it runs tests; the
//! newest copies the tests run from are kept. It refuses to run without a
//! readable `Cargo.lock` (every registry unit would look unlisted), or when
//! `CARGO_TARGET_DIR` is set but `--target-dir` is not: a target dir shared
//! with other workspaces would lose their registry units.
//!
//! `--all` cleans every place the workflow leaves build and test output:
//!
//! - this checkout's `target/`, as above;
//! - each other worktree's `target/`: removed whole once idle (no build for
//!   `--idle-hours`, default 2) and no running program lives in it, else
//!   cleaned as above. The main checkout's is only ever cleaned as above;
//! - Cargo target dirs (holding `CACHEDIR.TAG`) in this project's Claude
//!   scratchpads (`/tmp/claude-<uid>/<project>/`) and in the temp dir (those
//!   with ghostrealm units): removed whole once idle and unused;
//! - `ghostrealm-*` entries in the temp dir that tests and the web host leave
//!   when their process dies before cleaning up: removed once no process whose
//!   pid appears in the name is running and they are 5 minutes old. The app's
//!   frame directory `ghostrealm-<uid>` is kept;
//! - CEF's `<bundle id>.*` temp dirs (`dev.ghostrealm.web-host.*`): removed
//!   while no web host runs.
//!
//! Run it from the repo root with `mise run clean-stale [-- --dry-run]` (or
//! `mise run clean-all`), or build it with
//! `rustc -O --edition 2021 scripts/clean-stale.rs`. Options:
//! `--dry-run`/`-n` (remove nothing), `--verbose`/`-v` (list each path),
//! `--keep N`, `--target-dir DIR`, `--lockfile FILE`, `--all`,
//! `--idle-hours N`, `--except DIR` (leave that target dir alone; repeatable).
//! Its tests run with `rustc --test --edition 2021 scripts/clean-stale.rs`.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

/// The fewest incremental-compilation directories kept per crate name.
const KEEP_INCREMENTAL: usize = 4;

/// How old a temp-dir leftover must be before it goes: a test creating its
/// directory has a live pid, but the name may be read before it is written.
const TEMP_MIN_AGE: Duration = Duration::from_secs(5 * 60);

/// The bundle id CEF prefixes its temp dirs with.
const CEF_TEMP_PREFIX: &str = "dev.ghostrealm.web-host.";

struct Unit {
    package: String,
    hash: String,
    /// The fingerprint file's name: `lib-syn`, `test-bin-ghostrealm`, ...
    kind: String,
    built: SystemTime,
}

#[derive(Default)]
struct Totals {
    units: usize,
    removed: usize,
    bytes: u64,
}

struct Opts {
    dry_run: bool,
    verbose: bool,
    keep: usize,
    /// Target dirs left alone (canonical paths).
    except: Vec<PathBuf>,
}

impl Opts {
    fn freed(&self) -> &'static str {
        if self.dry_run { "would free" } else { "freed" }
    }

    fn excepts(&self, target: &Path) -> bool {
        fs::canonicalize(target).is_ok_and(|t| self.except.contains(&t))
    }
}

fn main() -> ExitCode {
    let mut opts = Opts { dry_run: false, verbose: false, keep: 3, except: Vec::new() };
    let mut target = PathBuf::from("target");
    let mut lockfile = PathBuf::from("Cargo.lock");
    let mut target_given = false;
    let mut all = false;
    let mut idle = Duration::from_secs(2 * 3600);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" | "-n" => opts.dry_run = true,
            "--verbose" | "-v" => opts.verbose = true,
            "--keep" => opts.keep = args.next().and_then(|n| n.parse().ok()).unwrap_or(opts.keep).max(1),
            "--target-dir" => {
                target = args.next().map(PathBuf::from).unwrap_or(target);
                target_given = true;
            }
            "--lockfile" => lockfile = args.next().map(PathBuf::from).unwrap_or(lockfile),
            "--all" => all = true,
            "--except" => opts.except.extend(args.next().and_then(|d| fs::canonicalize(d).ok())),
            "--idle-hours" => {
                let hours: Option<f64> = args.next().and_then(|n| n.parse().ok());
                idle = hours.map_or(idle, |h| Duration::from_secs_f64(h.max(0.0) * 3600.0));
            }
            other => {
                eprintln!("clean-stale: unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    if std::env::var_os("CARGO_TARGET_DIR").is_some() && !target_given {
        eprintln!("clean-stale: CARGO_TARGET_DIR is set; pass --target-dir (and --lockfile) explicitly");
        return ExitCode::from(2);
    }
    // Without the lockfile every registry unit would look unlisted.
    let Some(lock) = read_lock(&lockfile) else {
        eprintln!("clean-stale: no packages in {}; run it from the workspace root", lockfile.display());
        return ExitCode::from(2);
    };
    let mut total = clean_target(&target, &lock, &opts);
    if all {
        let running = Running::now();
        total += clean_worktrees(&target, idle, &running, &opts);
        total += clean_scratch_targets(idle, &running, &opts);
        total += clean_temp(&std::env::temp_dir(), idle, &running, &opts);
    }
    println!("{} {}", opts.freed(), human(total));
    ExitCode::SUCCESS
}

/// Remove the superseded units of `target`, unless a build holds it.
fn clean_target(target: &Path, lock: &Lock, opts: &Opts) -> u64 {
    if opts.excepts(target) {
        println!("{}: excepted; kept", target.display());
        return 0;
    }
    let Some(_held) = BuildLock::take(target) else {
        println!("{}: a build is running; skipped", target.display());
        return 0;
    };
    let mut bytes = 0;
    for profile in ["debug", "release"] {
        let dir = target.join(profile);
        if dir.is_dir() {
            let t = clean_profile(&dir, lock, opts);
            println!(
                "{}: {} of {} units superseded; {} {}",
                dir.display(),
                t.removed,
                t.units,
                opts.freed(),
                human(t.bytes)
            );
            bytes += t.bytes;
        }
    }
    bytes
}

/// Cargo's locks on a target dir, held exclusively. Cargo takes them for the
/// whole build (not while tests run), so holding them means no build is
/// writing and none can start.
struct BuildLock {
    _files: Vec<File>,
}

impl BuildLock {
    /// `None` while a build holds any of them.
    fn take(target: &Path) -> Option<BuildLock> {
        let mut held = Vec::new();
        for profile in ["debug", "release"] {
            for name in [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"] {
                let Ok(f) = File::open(target.join(profile).join(name)) else { continue };
                if f.try_lock().is_err() {
                    return None;
                }
                held.push(f);
            }
        }
        Some(BuildLock { _files: held })
    }
}

/// What is running now: each process's pid and executable path.
struct Running {
    pids: HashSet<u32>,
    programs: Vec<PathBuf>,
}

impl Running {
    fn now() -> Running {
        let out = Command::new("ps").args(["-axo", "pid=,comm="]).output();
        let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
        let mut running = Running { pids: HashSet::new(), programs: Vec::new() };
        for line in text.lines() {
            let line = line.trim_start();
            let (pid, comm) = line.split_once(' ').unwrap_or((line, ""));
            if let Ok(pid) = pid.parse() {
                running.pids.insert(pid);
            }
            running.programs.push(PathBuf::from(comm.trim()));
        }
        running
    }

    /// Whether a running program's executable is inside `dir` (a test binary,
    /// the app, or the bundled web host).
    fn uses(&self, dir: &Path) -> bool {
        let dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
        self.programs.iter().any(|p| p.starts_with(&dir))
    }

    /// Whether a web host (whose CEF temp dirs these are) is running.
    fn web_host(&self) -> bool {
        self.programs
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ghostrealm-web-host")))
    }
}

/// When a build last wrote to `target`: the newest of its profiles' lock
/// files and unit directories.
fn last_build(target: &Path) -> SystemTime {
    let mut newest = SystemTime::UNIX_EPOCH;
    for profile in ["debug", "release"] {
        for name in ["", ".cargo-lock", ".cargo-build-lock", "deps", ".fingerprint", "incremental", "build"] {
            let p = target.join(profile).join(name);
            if let Ok(t) = fs::metadata(&p).and_then(|m| m.modified()) {
                newest = newest.max(t);
            }
        }
    }
    newest
}

fn older_than(t: SystemTime, age: Duration) -> bool {
    SystemTime::now().duration_since(t).is_ok_and(|d| d >= age)
}

/// Remove a whole target dir once no build has written to it for `idle` and
/// no running program lives in it. Returns the bytes freed.
fn remove_idle_target(target: &Path, idle: Duration, running: &Running, opts: &Opts) -> u64 {
    if opts.excepts(target) {
        println!("{}: excepted; kept", target.display());
        return 0;
    }
    let Some(_held) = BuildLock::take(target) else {
        println!("{}: a build is running; kept", target.display());
        return 0;
    };
    if running.uses(target) {
        println!("{}: a program in it is running; kept", target.display());
        return 0;
    }
    if !older_than(last_build(target), idle) {
        println!("{}: built in the last {}; kept", target.display(), hours(idle));
        return 0;
    }
    let bytes = remove(target, opts);
    println!("{}: idle; {} {}", target.display(), opts.freed(), human(bytes));
    bytes
}

fn hours(d: Duration) -> String {
    format!("{:.1} h", d.as_secs_f64() / 3600.0)
}

/// `git worktree list`: the main checkout first, then the others.
fn worktrees() -> Vec<PathBuf> {
    let out = Command::new("git").args(["worktree", "list", "--porcelain"]).output();
    let text = out.map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default();
    text.lines().filter_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from).collect()
}

/// Every other worktree's `target/`: the main checkout's has its superseded
/// units removed; a linked worktree's goes whole once idle, else the same.
fn clean_worktrees(own_target: &Path, idle: Duration, running: &Running, opts: &Opts) -> u64 {
    let own = fs::canonicalize(own_target).ok();
    let mut bytes = 0;
    for (i, tree) in worktrees().into_iter().enumerate() {
        let target = tree.join("target");
        if !target.is_dir() || fs::canonicalize(&target).ok() == own {
            continue;
        }
        let freed = if i > 0 { remove_idle_target(&target, idle, running, opts) } else { 0 };
        if freed == 0 && target.is_dir() && !opts.excepts(&target) {
            match read_lock(&tree.join("Cargo.lock")) {
                Some(lock) => bytes += clean_target(&target, &lock, opts),
                None => println!("{}: no Cargo.lock beside it; kept", target.display()),
            }
        }
        bytes += freed;
    }
    bytes
}

/// This project's Claude scratchpads: `/tmp/claude-<uid>/<checkout path with
/// '/' as '-'>/`, one directory per session.
fn scratch_root() -> Option<PathBuf> {
    let main = worktrees().into_iter().next()?;
    let slug = main.to_string_lossy().replace('/', "-");
    let uid = Command::new("id").arg("-u").output().ok()?;
    let uid = String::from_utf8_lossy(&uid.stdout).trim().to_string();
    Some(PathBuf::from(format!("/tmp/claude-{uid}")).join(slug))
}

/// Cargo target dirs (marked by `CACHEDIR.TAG`) at most `depth` levels below
/// `dir`; a target dir's own contents are not searched.
fn find_targets(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    for e in read_dir(dir) {
        let p = e.path();
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if p.join("CACHEDIR.TAG").is_file() {
            found.push(p);
        } else if depth > 1 {
            find_targets(&p, depth - 1, found);
        }
    }
}

/// Idle target dirs in this project's scratchpads (reviewers build there with
/// their own `CARGO_TARGET_DIR`).
fn clean_scratch_targets(idle: Duration, running: &Running, opts: &Opts) -> u64 {
    let Some(root) = scratch_root() else { return 0 };
    let mut targets = Vec::new();
    find_targets(&root, 4, &mut targets);
    targets.iter().map(|t| remove_idle_target(t, idle, running, opts)).sum()
}

/// Whether a target dir holds units of this workspace.
fn has_ghostrealm_units(target: &Path) -> bool {
    ["debug", "release"].iter().any(|profile| {
        read_dir(&target.join(profile).join(".fingerprint"))
            .iter()
            .any(|e| e.file_name().to_string_lossy().starts_with("ghostrealm"))
    })
}

/// The temp dir: leftovers of tests and web hosts whose process is gone, CEF
/// temp dirs while no host runs, and idle ghostrealm target dirs.
fn clean_temp(temp: &Path, idle: Duration, running: &Running, opts: &Opts) -> u64 {
    let frame_dir = format!("ghostrealm-{}", unsafe { getuid() });
    let host = running.web_host();
    let (mut count, mut bytes) = (0, 0);
    for e in read_dir(temp) {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        let stale = if name.starts_with("ghostrealm-") {
            name != frame_dir && leftover_is_stale(&name, &path, &running.pids)
        } else if name.starts_with(CEF_TEMP_PREFIX) {
            !host
        } else {
            if path.join("CACHEDIR.TAG").is_file() && has_ghostrealm_units(&path) {
                bytes += remove_idle_target(&path, idle, running, opts);
            }
            false
        };
        if stale {
            count += 1;
            bytes += remove(&path, opts);
        }
    }
    println!("{}: {count} leftovers; {} {}", temp.display(), opts.freed(), human(bytes));
    bytes
}

/// A `ghostrealm-<tag>-<pid>[-<n>]` (or `...-<pid>.txt`) temp entry goes once
/// no number in its name is a running pid and it is [`TEMP_MIN_AGE`] old.
/// Counting the other numbers as pids too only keeps more.
fn leftover_is_stale(name: &str, path: &Path, pids: &HashSet<u32>) -> bool {
    let alive = name.split(['-', '.']).filter_map(|s| s.parse::<u32>().ok()).any(|n| pids.contains(&n));
    let modified = fs::symlink_metadata(path).and_then(|m| m.modified()).unwrap_or_else(|_| SystemTime::now());
    !alive && older_than(modified, TEMP_MIN_AGE)
}

extern "C" {
    fn getuid() -> u32;
}

/// What `Cargo.lock` lists.
#[derive(Default)]
struct Lock {
    /// `<name>-<version>` of each package, as registry source directories are named.
    dirs: HashSet<String>,
}

/// `None` if it can't be read or lists no packages.
fn read_lock(lockfile: &Path) -> Option<Lock> {
    let mut lock = Lock::default();
    let text = fs::read_to_string(lockfile).ok()?;
    let mut name = None;
    for line in text.lines() {
        let quoted = |key: &str| line.strip_prefix(key).and_then(|n| n.strip_suffix('"')).map(str::to_string);
        if let Some(n) = quoted("name = \"") {
            name = Some(n);
        } else if let (Some(v), Some(n)) = (quoted("version = \""), name.take()) {
            lock.dirs.insert(format!("{n}-{v}"));
        }
    }
    (!lock.dirs.is_empty()).then_some(lock)
}

/// Whether `source` is in a registry package directory `Cargo.lock` doesn't list.
fn unlisted_registry_source(source: &str, lock: &Lock) -> bool {
    let Some((_, rest)) = source.split_once("/registry/src/") else { return false };
    let mut parts = rest.split('/');
    parts.next(); // the registry index directory
    parts.next().is_some_and(|dir| !lock.dirs.contains(dir))
}

/// Split `<stem>-<16 hex digits>` followed by nothing or `.<anything>`.
fn split_hash(name: &str) -> Option<(&str, &str)> {
    let b = name.as_bytes();
    (0..b.len()).filter(|&i| b[i] == b'-').find_map(|i| {
        let hash = name.get(i + 1..i + 17)?;
        let ok = hash.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            && (b.len() == i + 17 || b[i + 17] == b'.');
        ok.then(|| (&name[..i], hash))
    })
}

fn clean_profile(dir: &Path, lock: &Lock, opts: &Opts) -> Totals {
    let mut totals = Totals::default();
    let deps = dir.join("deps");
    // hash -> the entries of deps/ belonging to that unit.
    let mut by_hash: HashMap<String, Vec<PathBuf>> = HashMap::new();
    for e in read_dir(&deps) {
        if let Some((_, hash)) = e.file_name().to_str().and_then(split_hash) {
            by_hash.entry(hash.to_string()).or_default().push(e.path());
        }
    }

    // identity -> units sharing it. Build-script runs are decided after the
    // build-script compiles they belong to.
    let mut groups: HashMap<String, (usize, Vec<Unit>)> = HashMap::new();
    let mut runs: Vec<(Unit, Vec<String>)> = Vec::new();
    // fingerprint hash -> `.fingerprint/` directory of each build-script compile.
    let mut scripts: HashMap<String, String> = HashMap::new();
    for e in read_dir(&dir.join(".fingerprint")) {
        let Some(dir_name) = e.file_name().to_str().map(str::to_string) else { continue };
        if let Ok(h) = fs::read_to_string(e.path().join("build-script-build-script-build")) {
            scripts.insert(h.trim().to_string(), dir_name.clone());
        }
        let Some(package) = dir_name.len().checked_sub(17).and_then(|n| dir_name.get(..n)) else { continue };
        let Some((_, hash)) = split_hash(&dir_name) else { continue };
        let Some((kind, json, built)) = fingerprint_json(&e.path()) else { continue };
        let unit = Unit { package: package.to_string(), hash: hash.to_string(), kind: kind.clone(), built };
        if kind.starts_with("run-build-script-") {
            runs.push((unit, script_fingerprints(&json)));
            continue;
        }
        let source = dep_info_source(by_hash.get(hash), &dir.join("build").join(&dir_name));
        let identity = format!(
            "{package}|{kind}|{}|{}|{}|{}|{source}",
            field(&json, "features"),
            field(&json, "target"),
            field(&json, "compile_kind"),
            field(&json, "rustflags"),
        );
        let keep = if unlisted_registry_source(&source, lock) { 0 } else { opts.keep };
        groups.entry(identity).or_insert((keep, Vec::new())).1.push(unit);
    }

    let mut kept: Vec<Unit> = Vec::new();
    let mut removed: HashSet<String> = HashSet::new();
    for (keep, mut units) in groups.into_values() {
        units.sort_by(|a, b| b.built.cmp(&a.built).then_with(|| a.hash.cmp(&b.hash)));
        for (i, unit) in units.into_iter().enumerate() {
            if i < keep {
                kept.push(unit);
            } else {
                removed.insert(format!("{}-{}", unit.package, unit.hash));
                totals.bytes += remove_unit(dir, &unit, &by_hash, opts);
            }
        }
    }

    // A build-script run (whose `out/` may hold a large download) goes with the
    // compile it ran: the run's fingerprint records the compile's fingerprint
    // hash (among those of the runs it reads `links` metadata from, which are
    // not compiles). A run whose compile is gone is superseded only once
    // another run of the package has a compile that stays; otherwise it is
    // left alone.
    let compile_of = |fps: &Vec<String>| fps.iter().find_map(|h| scripts.get(h));
    let current: HashSet<String> = runs
        .iter()
        .filter(|(_, fp)| compile_of(fp).is_some_and(|c| !removed.contains(c)))
        .map(|(u, _)| u.package.clone())
        .collect();
    for (unit, fp) in runs {
        let superseded = match compile_of(&fp) {
            Some(compile) => removed.contains(compile),
            None => current.contains(&unit.package),
        };
        if superseded {
            removed.insert(format!("{}-{}", unit.package, unit.hash));
            totals.bytes += remove_unit(dir, &unit, &by_hash, opts);
        } else {
            kept.push(unit);
        }
    }
    totals.removed = removed.len();
    totals.units = kept.len() + removed.len();

    // incremental/<crate>-<id>: one directory per compiled target, so a crate
    // keeps at least as many as it has units left (lib, bin, tests, each in
    // check and build mode).
    let mut crates: HashMap<String, Vec<(SystemTime, PathBuf)>> = HashMap::new();
    for e in read_dir(&dir.join("incremental")) {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((krate, _)) = name.rsplit_once('-') else { continue };
        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        crates.entry(krate.to_string()).or_default().push((modified, e.path()));
    }
    let kinds: Vec<String> = kept.iter().map(|u| format!("_{}", u.kind.replace('-', "_"))).collect();
    for (krate, mut dirs) in crates {
        // `_lib_foo` and `_test_bin_foo` end in `_foo`. Overcounting (a `foo`
        // that is the tail of another crate's name) only keeps more.
        let suffix = format!("_{krate}");
        let units = kinds.iter().filter(|k| k.ends_with(&suffix)).count();
        dirs.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, p) in dirs.into_iter().skip(units.max(KEEP_INCREMENTAL)) {
            totals.bytes += remove(&p, opts);
        }
    }
    totals
}

/// Remove a unit's `.fingerprint/` and `build/` directories and `deps/` files.
fn remove_unit(dir: &Path, unit: &Unit, by_hash: &HashMap<String, Vec<PathBuf>>, opts: &Opts) -> u64 {
    let name = format!("{}-{}", unit.package, unit.hash);
    let mut paths = vec![dir.join(".fingerprint").join(&name), dir.join("build").join(&name)];
    paths.extend(by_hash.get(&unit.hash).cloned().unwrap_or_default());
    paths.iter().map(|p| remove(p, opts)).sum()
}

/// The fingerprint hashes, as `.fingerprint/` stores them (16 hex digits of
/// the little-endian `u64`), of the `build_script_build` units a run's
/// fingerprint JSON depends on: `"deps":[[<id>,"build_script_build",<public>,<hash>],...]`.
/// One is the run's own build-script compile; the others are the runs of
/// `links` dependencies (whose metadata it reads), in no particular order.
fn script_fingerprints(json: &str) -> Vec<String> {
    let mut hashes = Vec::new();
    let mut rest = json;
    while let Some(i) = rest.find("\"build_script_build\",") {
        rest = &rest[i + 1..];
        let hash = (|| {
            let r = &rest[rest.find(',')? + 1..];
            let r = &r[r.find(',')? + 1..];
            let digits = &r[..r.find(|c: char| !c.is_ascii_digit())?];
            let n: u64 = digits.parse().ok()?;
            Some(n.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect())
        })();
        hashes.extend(hash);
    }
    hashes
}

fn read_dir(dir: &Path) -> Vec<fs::DirEntry> {
    fs::read_dir(dir).map(|rd| rd.flatten().collect()).unwrap_or_default()
}

/// The fingerprint file of a unit: its kind (`lib-syn`, `test-bin-ghostrealm`,
/// `run-build-script-build-script-build`, ...), contents, and when it was written.
fn fingerprint_json(dir: &Path) -> Option<(String, String, SystemTime)> {
    let entry = read_dir(dir)
        .into_iter()
        .find(|e| e.file_name().to_string_lossy().ends_with(".json"))?;
    let kind = entry.file_name().to_string_lossy().trim_end_matches(".json").to_string();
    let json = fs::read_to_string(entry.path()).ok()?;
    let built = entry.metadata().and_then(|m| m.modified()).ok()?;
    Some((kind, json, built))
}

/// The raw value of top-level `"key":` in the fingerprint JSON (a string,
/// array, or number), or empty.
fn field<'a>(json: &'a str, key: &str) -> &'a str {
    let pat = format!("\"{key}\":");
    let Some(start) = json.find(&pat).map(|i| i + pat.len()) else { return "" };
    let rest = &json[start..];
    let end = match rest.as_bytes().first() {
        Some(b'"') => {
            let mut esc = false;
            rest.char_indices()
                .skip(1)
                .find(|&(_, c)| {
                    let done = c == '"' && !esc;
                    esc = c == '\\' && !esc;
                    done
                })
                .map_or(rest.len(), |(i, _)| i + 1)
        }
        Some(b'[') => rest.find(']').map_or(rest.len(), |i| i + 1),
        _ => rest.find([',', '}']).unwrap_or(rest.len()),
    };
    &rest[..end]
}

/// The first source file in the unit's dep-info (`.d`): the crate root, or the
/// build script.
fn dep_info_source(deps_files: Option<&Vec<PathBuf>>, build_dir: &Path) -> String {
    let in_build = read_dir(build_dir).into_iter().map(|e| e.path());
    let in_deps = deps_files.into_iter().flatten().cloned();
    for p in in_deps.chain(in_build) {
        if p.extension().is_some_and(|x| x == "d") {
            let first = fs::read_to_string(&p).ok().and_then(|t| t.lines().next().map(str::to_string));
            if let Some(src) = first.and_then(|l| l.split_once(": ").map(|(_, s)| s.to_string())) {
                return src.split(' ').next().unwrap_or("").to_string();
            }
        }
    }
    String::new()
}

/// Remove `path` (unless dry-running). Returns its disk size.
fn remove(path: &Path, opts: &Opts) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    let size = disk_size(path, &meta);
    if opts.verbose {
        println!("{} {} ({})", if opts.dry_run { "would remove" } else { "remove" }, path.display(), human(size));
    }
    if !opts.dry_run {
        let _ = if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) };
    }
    size
}

fn disk_size(path: &Path, meta: &fs::Metadata) -> u64 {
    let own = meta.blocks() * 512;
    if !meta.is_dir() {
        return own;
    }
    own + read_dir(path)
        .iter()
        .filter_map(|e| fs::symlink_metadata(e.path()).ok().map(|m| disk_size(&e.path(), &m)))
        .sum::<u64>()
}

fn human(bytes: u64) -> String {
    format!("{:.2} GiB", bytes as f64 / (1u64 << 30) as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fingerprint directory `<package>-<hash>` with `<kind>.json` and, for a
    /// build-script compile, the hash file a run's dependency names it by.
    fn unit(dir: &Path, name: &str, kind: &str, json: &str, script_hash: Option<u64>) {
        let fp = dir.join(".fingerprint").join(name);
        fs::create_dir_all(&fp).unwrap();
        fs::write(fp.join(format!("{kind}.json")), json).unwrap();
        if let Some(h) = script_hash {
            let hex: String = h.to_le_bytes().iter().map(|b| format!("{b:02x}")).collect();
            fs::write(fp.join(kind), hex).unwrap();
        }
        fs::create_dir_all(dir.join("build").join(name)).unwrap();
    }

    #[test]
    fn a_temp_leftover_goes_once_its_process_is_gone_and_it_is_old() {
        let dir = std::env::temp_dir().join(format!("clean-stale-leftover-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let old = SystemTime::now() - TEMP_MIN_AGE - Duration::from_secs(1);
        let entry = |name: &str, modified: SystemTime| {
            let p = dir.join(name);
            File::create(&p).unwrap().set_modified(modified).unwrap();
            p
        };
        let pids: HashSet<u32> = [4242].into();
        let dead = entry("ghostrealm-fb-1111-3", old);
        assert!(leftover_is_stale("ghostrealm-fb-1111-3", &dead, &pids));
        let alive = entry("ghostrealm-fb-4242-3", old);
        assert!(!leftover_is_stale("ghostrealm-fb-4242-3", &alive, &pids), "its test is running");
        let file = entry("ghostrealm-undo-4242.txt", old);
        assert!(!leftover_is_stale("ghostrealm-undo-4242.txt", &file, &pids), "a pid before an extension");
        let new = entry("ghostrealm-fb-1111-4", SystemTime::now());
        assert!(!leftover_is_stale("ghostrealm-fb-1111-4", &new, &pids), "too new");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_target_held_by_a_build_is_not_taken() {
        let dir = std::env::temp_dir().join(format!("clean-stale-lock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("debug")).unwrap();
        let build = File::create(dir.join("debug/.cargo-build-lock")).unwrap();
        build.lock().unwrap();
        assert!(BuildLock::take(&dir).is_none(), "a build holds it");
        build.unlock().unwrap();
        let held = BuildLock::take(&dir).expect("free once the build is done");
        assert!(File::open(dir.join("debug/.cargo-build-lock")).unwrap().try_lock().is_err(), "held while cleaning");
        drop(held);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_run_goes_with_its_compile_whatever_order_its_links_deps_come_in() {
        let dir = std::env::temp_dir().join(format!("clean-stale-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let compile = |features: &str| format!(r#"{{"features":"{features}","target":1,"deps":[]}}"#);
        // Two configurations of `pkg`, each a compile and its run. The run of
        // one lists a `links` dependency's run before its own compile.
        unit(&dir, "pkg-00000000000000a1", "build-script-build-script-build", &compile("a"), Some(11));
        unit(&dir, "pkg-00000000000000b1", "build-script-build-script-build", &compile("b"), Some(22));
        let run = |deps: &str| format!(r#"{{"features":"","target":0,"deps":[{deps}],"local":[]}}"#);
        let kind = "run-build-script-build-script-build";
        unit(&dir, "pkg-00000000000000a2", kind, &run(r#"[1,"build_script_build",false,11]"#), None);
        unit(
            &dir,
            "pkg-00000000000000b2",
            kind,
            &run(r#"[0,"build_script_build",false,99],[1,"build_script_build",false,22]"#),
            None,
        );
        let mut lock = Lock::default();
        lock.dirs.insert("pkg-1.0.0".into());
        let t = clean_profile(&dir, &lock, &Opts { dry_run: false, verbose: false, keep: 3, except: Vec::new() });
        assert_eq!(t.removed, 0);
        assert!(dir.join("build/pkg-00000000000000b2").is_dir(), "the run's output stays");
        let _ = fs::remove_dir_all(&dir);
    }
}
