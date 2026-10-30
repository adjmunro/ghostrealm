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
//! dir is skipped while a build holds Cargo's locks on its `debug` or
//! `release` dir (`.cargo-lock`, `.cargo-build-lock`, `.cargo-artifact-lock`),
//! or one can't be opened, and they are held while cleaning, so a build that
//! starts meanwhile waits. Missing lock files are created (with their profile
//! dir), except with `--dry-run`, which creates nothing and locks only the
//! lock files there are. A profile dir that is a symlink is neither locked
//! nor cleaned. Cargo releases the locks before it runs tests; the newest
//! copies the tests run from are kept. It refuses to run without a
//! readable `Cargo.lock` (every registry unit would look unlisted), or when
//! `CARGO_TARGET_DIR` is set but `--target-dir` is not: a target dir shared
//! with other workspaces would lose their registry units.
//!
//! `--all` cleans every place the workflow leaves build and test output:
//!
//! - this checkout's `target/`, as above;
//! - each other worktree's `target/`. An agent worktree's (directly under
//!   the main checkout's `.claude/worktrees/`) is emptied once idle and no
//!   running program lives in it, else cleaned as above. Every other
//!   worktree's, the main checkout's included, is only cleaned as above;
//! - Cargo target dirs (holding `CACHEDIR.TAG`) in this project's Claude
//!   scratchpads (`/tmp/claude-<uid>/<project>/`) and in the temp dir (those
//!   named `ghostrealm-*` or with ghostrealm units): emptied once idle and no
//!   running program lives in them;
//! - other `ghostrealm-*` entries in the temp dir that tests and the web host
//!   leave when their process dies before cleaning up: removed once the
//!   process whose pid the name carries (its first number) is gone and they
//!   are 5 minutes old. The app's frame directory `ghostrealm-<uid>` and its
//!   fallbacks `ghostrealm-<uid>-*` are kept (the web host clears those);
//! - the web hosts' Chromium temp dirs `ghostrealm-<uid>-cef/<pid>`: removed
//!   once that pid is gone and they are 5 minutes old (the next host also
//!   clears them);
//! - CEF's `<bundle id>.*` temp dirs (`dev.ghostrealm.web-host.*`), which
//!   Chromium makes in the temp dir when a host has no dir of its own for
//!   them: removed once 5 minutes old while no web host runs.
//!
//! The target dir given (or `target`) is never emptied, and temp-dir
//! entries of other users are not touched. Emptying holds the `debug` and
//! `release` locks and keeps `CACHEDIR.TAG` and those lock files, so a build
//! queued on a lock while it is emptied still excludes the next. Any other
//! dir in it with lock files (a custom profile's or a target triple's) is
//! kept whole, as is one modified since the locks were taken (a build may
//! have created it and not yet its lock file) and a symlinked `debug` or
//! `release` (unlocked: a build may be using it).
//!
//! Idle means `--idle-hours` (default 2) have passed since a unit dir
//! (`deps`, `.fingerprint`, `incremental`, `build`) in `debug` or `release`,
//! or `.rustc_info.json`, was modified or any executable in those profiles
//! was run (by its access time, where the filesystem records one), and, for
//! a worktree, since its git index or `HEAD` changed. Cargo writes nothing
//! to a target dir when it re-runs tests without compiling, so a filesystem
//! mounted `noatime` makes such a worktree look idle; raise `--idle-hours`
//! there.
//!
//! What runs is read from the kernel on macOS (`proc_pidpath`) just before
//! each emptying it guards, and once for the temp dir, whose entry ages are
//! measured to that reading. A process whose path can't be read counts as
//! another user's only when its owner is read (`sysctl` `KERN_PROC_PID`) and
//! differs, or it is gone. Every target dir counts as in use, and a web host
//! as running, if the process list can't be read or lacks this process, if
//! this process's own owner can't be read, or if any other process may be
//! this user's and has no readable path. Other systems don't read what runs,
//! so there `--all` empties no target dir and removes nothing from the temp
//! dir. A path that can't be removed is reported and the exit status is 1.
//!
//! Run it from the repo root with `mise run clean-stale [-- --dry-run]` (or
//! `mise run clean-all`), or build it with
//! `rustc -O --edition 2021 scripts/clean-stale.rs`. Options:
//! `--dry-run`/`-n` (remove nothing), `--verbose`/`-v` (list each path),
//! `--keep N`, `--target-dir DIR`, `--lockfile FILE`, `--all`,
//! `--idle-hours N`, `--except DIR` (leave that target dir alone; repeatable).
//! An option value that doesn't parse, or an `--except` dir that doesn't
//! exist, is an error. Its tests run with
//! `rustc --test --edition 2021 scripts/clean-stale.rs`.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
#[cfg(target_os = "macos")]
use std::os::raw::c_int;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, SystemTime};

/// The fewest incremental-compilation directories kept per crate name.
const KEEP_INCREMENTAL: usize = 4;

/// How old a temp-dir leftover must be before it goes: a test creating its
/// directory has a live pid, but the name may be read before it is written;
/// a CEF temp dir may belong to a host that started after the process list
/// was read.
const TEMP_MIN_AGE: Duration = Duration::from_secs(5 * 60);

/// Cargo's lock files in a profile dir.
const LOCK_FILES: [&str; 3] = [".cargo-lock", ".cargo-build-lock", ".cargo-artifact-lock"];

/// How much earlier than the locks were taken a dir's modification time may
/// be and still count as since: some filesystems keep whole seconds.
const MTIME_SLACK: Duration = Duration::from_secs(2);

/// The bundle id CEF prefixes its temp dirs with, in the temp dir when the
/// host has not pointed Chromium's own (`ghostrealm_web::temp`) elsewhere.
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

#[derive(Default)]
struct Opts {
    dry_run: bool,
    verbose: bool,
    keep: usize,
    /// Target dirs left alone (canonical paths).
    except: Vec<PathBuf>,
    /// Paths that could not be removed.
    failures: Cell<usize>,
    /// The target dir this run cleans (canonical): never emptied.
    own: Option<PathBuf>,
}

impl Opts {
    fn freed(&self) -> &'static str {
        if self.dry_run { "would free" } else { "freed" }
    }

    fn excepts(&self, target: &Path) -> bool {
        fs::canonicalize(target).is_ok_and(|t| self.except.contains(&t))
    }

    fn is_own(&self, target: &Path) -> bool {
        self.own.is_some() && fs::canonicalize(target).ok() == self.own
    }
}

struct Args {
    opts: Opts,
    target: PathBuf,
    /// Whether `--target-dir` was given.
    target_given: bool,
    lockfile: PathBuf,
    all: bool,
    idle: Duration,
}

fn parse_args(mut args: impl Iterator<Item = String>) -> Result<Args, String> {
    let mut a = Args {
        opts: Opts { keep: 3, ..Opts::default() },
        target: PathBuf::from("target"),
        target_given: false,
        lockfile: PathBuf::from("Cargo.lock"),
        all: false,
        idle: Duration::from_secs(2 * 3600),
    };
    while let Some(arg) = args.next() {
        let mut value = || args.next().ok_or(format!("{arg} needs a value"));
        match arg.as_str() {
            "--dry-run" | "-n" => a.opts.dry_run = true,
            "--verbose" | "-v" => a.opts.verbose = true,
            "--keep" => {
                let v = value()?;
                a.opts.keep = v.parse().ok().filter(|&n| n >= 1).ok_or(format!("--keep {v}: not a count of 1 or more"))?;
            }
            "--target-dir" => {
                a.target = PathBuf::from(value()?);
                a.target_given = true;
            }
            "--lockfile" => a.lockfile = PathBuf::from(value()?),
            "--all" => a.all = true,
            "--except" => {
                let v = value()?;
                a.opts.except.push(fs::canonicalize(&v).map_err(|e| format!("--except {v}: {e}"))?);
            }
            "--idle-hours" => {
                let v = value()?;
                a.idle = v
                    .parse::<f64>()
                    .ok()
                    .filter(|h| *h >= 0.0)
                    .and_then(|h| Duration::try_from_secs_f64(h * 3600.0).ok())
                    .ok_or(format!("--idle-hours {v}: not a number of hours"))?;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn main() -> ExitCode {
    let Args { mut opts, target, target_given, lockfile, all, idle } = match parse_args(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("clean-stale: {e}");
            return ExitCode::from(2);
        }
    };
    if std::env::var_os("CARGO_TARGET_DIR").is_some() && !target_given {
        eprintln!("clean-stale: CARGO_TARGET_DIR is set; pass --target-dir (and --lockfile) explicitly");
        return ExitCode::from(2);
    }
    // Without the lockfile every registry unit would look unlisted.
    let Some(lock) = read_lock(&lockfile) else {
        eprintln!("clean-stale: no packages in {}; run it from the workspace root", lockfile.display());
        return ExitCode::from(2);
    };
    opts.own = fs::canonicalize(&target).ok();
    let mut total = clean_target(&target, &lock, &opts);
    if all {
        let trees = worktrees();
        total += clean_worktrees(&trees, idle, &opts);
        total += clean_scratch_targets(&trees, idle, &opts);
        let temp = std::env::temp_dir();
        match Running::now() {
            Some(running) => total += clean_temp(&temp, unsafe { getuid() }, idle, &running, &opts),
            None => println!("{}: can't tell what is running; skipped", temp.display()),
        }
    }
    println!("{} {}", opts.freed(), human(total));
    match opts.failures.get() {
        0 => ExitCode::SUCCESS,
        n => {
            eprintln!("clean-stale: {n} paths could not be removed");
            ExitCode::FAILURE
        }
    }
}

/// Remove the superseded units of `target`, unless a build holds it.
fn clean_target(target: &Path, lock: &Lock, opts: &Opts) -> u64 {
    if opts.excepts(target) {
        println!("{}: excepted; kept", target.display());
        return 0;
    }
    let _held = match BuildLock::take(target, opts.dry_run) {
        Ok(held) => held,
        Err(why) => {
            println!("{}: {why}; skipped", target.display());
            return 0;
        }
    };
    let mut bytes = 0;
    for profile in ["debug", "release"] {
        let dir = target.join(profile);
        // A symlinked profile isn't locked: a build in its destination isn't seen.
        if real_dir(&dir) {
            let t = clean_profile(&dir, lock, opts);
            if t.units == 0 && t.bytes == 0 {
                continue;
            }
            if t.units == 0 {
                println!("{}: incremental dirs trimmed; {} {}", dir.display(), opts.freed(), human(t.bytes));
            } else {
                println!(
                    "{}: {} of {} units superseded; {} {}",
                    dir.display(),
                    t.removed,
                    t.units,
                    opts.freed(),
                    human(t.bytes)
                );
            }
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
    /// Just before the first lock was taken.
    at: SystemTime,
}

/// Why a target dir's locks weren't taken.
#[derive(Debug)]
enum NotTaken {
    /// A build holds one.
    Held,
    /// One can't be opened or created.
    Unopenable(PathBuf, io::Error),
}

impl std::fmt::Display for NotTaken {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            NotTaken::Held => write!(f, "a build is running"),
            NotTaken::Unopenable(p, e) => write!(f, "can't lock {}: {e}", p.display()),
        }
    }
}

impl BuildLock {
    /// The locks of the `debug` and `release` dirs that are real directories
    /// (a symlinked one's lock files are in another target). Existing lock
    /// files are opened read-only (`flock` needs no write access). Unless
    /// `dry_run`, missing profile dirs and lock files are created, so a build
    /// that starts meanwhile in a profile not built before waits too;
    /// `dry_run` creates nothing.
    fn take(target: &Path, dry_run: bool) -> Result<BuildLock, NotTaken> {
        let at = SystemTime::now();
        let mut held = Vec::new();
        for profile in ["debug", "release"] {
            let dir = target.join(profile);
            if !dry_run && fs::symlink_metadata(&dir).is_err() {
                fs::create_dir_all(&dir).map_err(|e| NotTaken::Unopenable(dir.clone(), e))?;
            }
            if !real_dir(&dir) {
                continue;
            }
            for name in LOCK_FILES {
                let path = dir.join(name);
                let f = match File::open(&path) {
                    Err(e) if e.kind() == io::ErrorKind::NotFound && dry_run => continue,
                    // `create_new` doesn't follow a dangling symlink; a build
                    // that created the file meanwhile leaves it to open.
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        match fs::OpenOptions::new().write(true).create_new(true).open(&path) {
                            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => File::open(&path),
                            other => other,
                        }
                    }
                    other => other,
                };
                let f = f.map_err(|e| NotTaken::Unopenable(path.clone(), e))?;
                match f.try_lock() {
                    Ok(()) => held.push(f),
                    Err(fs::TryLockError::WouldBlock) => return Err(NotTaken::Held),
                    Err(fs::TryLockError::Error(e)) => return Err(NotTaken::Unopenable(path, e)),
                }
            }
        }
        Ok(BuildLock { _files: held, at })
    }
}

/// Whether `path` is a directory and not a symlink.
fn real_dir(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_dir())
}

/// What is running now: each process's pid and executable path.
struct Running {
    /// When the list was read.
    at: SystemTime,
    pids: HashSet<u32>,
    /// The executable paths, as the kernel reports them and canonical.
    programs: Vec<PathBuf>,
    /// A process that may be this user's has no known absolute executable
    /// path: it may run from anywhere.
    unknown: bool,
}

/// A process's executable.
enum Exe {
    /// Read only on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    At(PathBuf),
    /// Another user's process, or one that exited since it was listed.
    Other,
    /// Path unreadable, and the process may be this user's.
    Unknown,
}

/// Who owns a process whose executable path couldn't be read.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
enum Owner {
    /// Its real and effective uids.
    Uids(Vec<u32>),
    /// It exited since it was listed.
    Gone,
    /// Not readable (a sandbox can deny it).
    Unknown,
}

impl Running {
    /// `None` if the process list can't be read or is implausible.
    fn now() -> Option<Running> {
        let at = SystemTime::now();
        let me = unsafe { getuid() };
        // An owner misread would count this user's processes as another's.
        match owner(std::process::id()) {
            Owner::Uids(uids) if uids.contains(&me) => {}
            _ => return None,
        }
        let pids = list_pids()?;
        Running::from_processes(at, pids.into_iter().map(|pid| (pid, exe(pid, me))))
    }

    /// `None` unless the list holds this process and another: a list without
    /// them was cut short, and anything missing from it could be in use.
    fn from_processes(at: SystemTime, processes: impl IntoIterator<Item = (u32, Exe)>) -> Option<Running> {
        let mut running = Running { at, pids: HashSet::new(), programs: Vec::new(), unknown: false };
        for (pid, exe) in processes {
            running.pids.insert(pid);
            match exe {
                Exe::At(p) if p.is_absolute() => {
                    running.programs.push(canonical(&p));
                    running.programs.push(p);
                }
                Exe::At(_) | Exe::Unknown => running.unknown = true,
                Exe::Other => {}
            }
        }
        (running.pids.contains(&std::process::id()) && running.pids.len() >= 2).then_some(running)
    }

    /// Whether a running program's executable may be inside `dir` (a test
    /// binary, the app, or the bundled web host).
    fn uses(&self, dir: &Path) -> bool {
        if self.unknown {
            return true;
        }
        let Ok(canonical) = fs::canonicalize(dir) else { return true };
        self.programs.iter().any(|p| inside(p, &canonical) || inside(p, dir))
    }

    /// Whether a web host (whose CEF temp dirs these are) may be running.
    fn web_host(&self) -> bool {
        self.unknown
            || self
                .programs
                .iter()
                .any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("ghostrealm-web-host")))
    }

    /// Whether `t` was `age` before the list was read: whatever started
    /// since is newer.
    fn older_than(&self, t: SystemTime, age: Duration) -> bool {
        self.at.duration_since(t).is_ok_and(|d| d >= age)
    }
}

/// `path` with its longest existing ancestor canonical: the executable of a
/// running program may have been removed or replaced since it started.
fn canonical(path: &Path) -> PathBuf {
    for base in path.ancestors() {
        if let Ok(c) = fs::canonicalize(base) {
            return c.join(path.strip_prefix(base).unwrap_or(Path::new("")));
        }
    }
    path.to_path_buf()
}

/// Whether `path` is `dir` or below it. macOS file systems are usually
/// case-insensitive, so there case is ignored.
fn inside(path: &Path, dir: &Path) -> bool {
    if cfg!(target_os = "macos") {
        let lower = |p: &Path| PathBuf::from(p.to_string_lossy().to_lowercase());
        lower(path).starts_with(lower(dir))
    } else {
        path.starts_with(dir)
    }
}

/// A process whose path couldn't be read is another's only when its owner
/// is known and isn't `me`, or it is gone.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn unresolved(owner: Owner, me: u32) -> Exe {
    match owner {
        Owner::Uids(uids) if !uids.is_empty() && !uids.contains(&me) => Exe::Other,
        Owner::Gone => Exe::Other,
        _ => Exe::Unknown,
    }
}

/// Whether `pid` no longer exists (and not just can't be signalled).
#[cfg(target_os = "macos")]
fn gone(pid: u32) -> bool {
    const ESRCH: i32 = 3;
    let Ok(pid) = c_int::try_from(pid) else { return false };
    pid > 0 && unsafe { kill(pid, 0) } != 0 && std::io::Error::last_os_error().raw_os_error() == Some(ESRCH)
}

#[cfg(target_os = "macos")]
fn list_pids() -> Option<Vec<u32>> {
    extern "C" {
        fn proc_listallpids(buffer: *mut std::ffi::c_void, buffersize: c_int) -> c_int;
    }
    let estimate = unsafe { proc_listallpids(std::ptr::null_mut(), 0) };
    let mut capacity = usize::try_from(estimate).ok().filter(|&n| n > 0)? * 2 + 64;
    // A full buffer may have been cut short: retry with a bigger one.
    for _ in 0..4 {
        let mut buf: Vec<c_int> = vec![0; capacity];
        let bytes = c_int::try_from(capacity * size_of::<c_int>()).ok()?;
        let n = usize::try_from(unsafe { proc_listallpids(buf.as_mut_ptr().cast(), bytes) }).ok()?;
        if n > 0 && n < capacity {
            buf.truncate(n);
            // 0 is the kernel.
            return Some(buf.into_iter().filter(|&p| p > 0).map(|p| p as u32).collect());
        }
        capacity *= 2;
    }
    None
}

#[cfg(target_os = "macos")]
fn exe(pid: u32, me: u32) -> Exe {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;
    extern "C" {
        fn proc_pidpath(pid: c_int, buffer: *mut std::ffi::c_void, buffersize: u32) -> c_int;
    }
    // PROC_PIDPATHINFO_MAXSIZE
    let mut buf = vec![0u8; 4096];
    let n = unsafe { proc_pidpath(pid as c_int, buf.as_mut_ptr().cast(), buf.len() as u32) };
    match usize::try_from(n) {
        Ok(n) if n > 0 => {
            buf.truncate(n);
            Exe::At(PathBuf::from(OsString::from_vec(buf)))
        }
        _ => unresolved(owner(pid), me),
    }
}

/// From `sysctl` `KERN_PROC_PID`, which reads any user's process (unlike
/// `proc_pidinfo`). The offsets are of `struct kinfo_proc` (64-bit); its pid
/// field is checked against `pid`.
#[cfg(target_os = "macos")]
fn owner(pid: u32) -> Owner {
    extern "C" {
        fn sysctl(
            name: *mut c_int,
            namelen: u32,
            oldp: *mut std::ffi::c_void,
            oldlenp: *mut usize,
            newp: *mut std::ffi::c_void,
            newlen: usize,
        ) -> c_int;
    }
    const SIZE: usize = 648; // sizeof(struct kinfo_proc)
    const PID: usize = 40; // kp_proc.p_pid
    const RUID: usize = 392; // kp_eproc.e_pcred.p_ruid
    const UID: usize = 420; // kp_eproc.e_ucred.cr_uid
    const _: () = assert!(size_of::<usize>() == 8);
    let Ok(id) = c_int::try_from(pid) else { return Owner::Unknown };
    // CTL_KERN, KERN_PROC, KERN_PROC_PID
    let mut mib = [1, 14, 1, id];
    let mut buf = [0u8; SIZE];
    let mut len = SIZE;
    let r = unsafe { sysctl(mib.as_mut_ptr(), 4, buf.as_mut_ptr().cast(), &mut len, std::ptr::null_mut(), 0) };
    let at = |o: usize| u32::from_ne_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]);
    match (r, len) {
        (0, SIZE) if at(PID) == pid => Owner::Uids(vec![at(RUID), at(UID)]),
        (0, 0) if gone(pid) => Owner::Gone,
        _ => Owner::Unknown,
    }
}

/// Elsewhere what runs is unknown: every check that needs it keeps. On
/// Linux `/proc` would serve, but a sandbox or container's own procfs shows
/// only its pid namespace, and that isn't detected yet (see `docs/TODO.md`).
#[cfg(not(target_os = "macos"))]
fn list_pids() -> Option<Vec<u32>> {
    None
}

#[cfg(not(target_os = "macos"))]
fn exe(_pid: u32, _me: u32) -> Exe {
    Exe::Unknown
}

#[cfg(not(target_os = "macos"))]
fn owner(_pid: u32) -> Owner {
    Owner::Unknown
}

/// When anything last happened in `target`: the newest modification of its
/// profiles' unit directories and of `.rustc_info.json`, and the newest
/// access of the executables in the profiles (running one records it). Not
/// the target or profile dirs themselves, nor the lock files: taking the
/// locks creates any that are missing.
fn last_active(target: &Path) -> SystemTime {
    let mut newest = mtime(&target.join(".rustc_info.json"));
    for profile in ["debug", "release"] {
        let dir = target.join(profile);
        for name in ["deps", ".fingerprint", "incremental", "build"] {
            newest = newest.max(mtime(&dir.join(name)));
        }
        for exes in [dir.clone(), dir.join("deps"), dir.join("examples")] {
            for e in read_dir(&exes) {
                // `DirEntry::metadata` doesn't follow symlinks.
                let Ok(m) = e.metadata() else { continue };
                if m.is_file() && m.mode() & 0o111 != 0 {
                    newest = newest.max(m.accessed().unwrap_or(SystemTime::UNIX_EPOCH));
                }
            }
        }
    }
    newest
}

/// When a linked worktree's git state last changed (a commit, checkout,
/// staging or status refresh): its admin dir's `HEAD`, `index` and `logs/HEAD`.
fn git_active(tree: &Path) -> SystemTime {
    let Ok(link) = fs::read_to_string(tree.join(".git")) else { return mtime(&tree.join(".git")) };
    let Some(admin) = link.trim().strip_prefix("gitdir: ").map(|d| tree.join(d)) else {
        return SystemTime::UNIX_EPOCH;
    };
    ["HEAD", "index", "logs/HEAD"].iter().map(|f| mtime(&admin.join(f))).max().unwrap_or(SystemTime::UNIX_EPOCH)
}

fn mtime(path: &Path) -> SystemTime {
    fs::symlink_metadata(path).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH)
}

fn older_than(t: SystemTime, age: Duration) -> bool {
    SystemTime::now().duration_since(t).is_ok_and(|d| d >= age)
}

/// What became of a target dir [`remove_idle_target`] looked at.
#[derive(Debug, PartialEq)]
enum Idle {
    /// Emptied; the bytes freed (or that would be).
    Emptied(u64),
    /// Kept for a reason other than its locks: it may still be cleaned.
    Kept,
    /// Its locks weren't taken: nothing else may touch it in this run.
    Locked,
}

/// Empty a target dir once it and `also` (other activity of its owner) are
/// idle and no running program lives in it. Every outcome but
/// [`Idle::Emptied`] with nothing freed prints one line.
fn remove_idle_target(target: &Path, also: SystemTime, idle: Duration, opts: &Opts) -> Idle {
    let kept = |why: &str| {
        println!("{}: {why}; kept", target.display());
        Idle::Kept
    };
    if opts.excepts(target) {
        return kept("excepted");
    }
    if opts.is_own(target) {
        return kept("the target dir this run cleans");
    }
    if !fs::symlink_metadata(target).is_ok_and(|m| m.is_dir()) {
        return kept("not a directory");
    }
    // Where what runs can't be read (any system but macOS), before the locks
    // are taken: taking them creates missing lock files.
    if Running::now().is_none() {
        return kept("can't tell what is running");
    }
    let held = match BuildLock::take(target, opts.dry_run) {
        Ok(held) => held,
        Err(why) => {
            println!("{}: {why}; skipped", target.display());
            return Idle::Locked;
        }
    };
    // Read after the lock is taken: no build can start a program from it now.
    let Some(running) = Running::now() else { return kept("can't tell what is running") };
    if running.uses(target) {
        return kept("a program in it may be running");
    }
    if !older_than(last_active(target).max(also), idle) {
        return kept(&format!("active in the last {}", hours(idle)));
    }
    let bytes = empty_target(target, held.at, opts);
    if bytes > 0 {
        println!("{}: idle; {} {}", target.display(), opts.freed(), human(bytes));
    }
    Idle::Emptied(bytes)
}

fn emptied_bytes(idle: Idle) -> u64 {
    match idle {
        Idle::Emptied(bytes) => bytes,
        Idle::Kept | Idle::Locked => 0,
    }
}

/// Remove everything in `target` but its `CACHEDIR.TAG`, the `debug` and
/// `release` lock files (held by the caller), a symlinked `debug` or
/// `release` (not locked, so a build may be using it), any other dir with
/// lock files, and any other dir modified since `locked` (when the caller
/// took the locks): a build creates a custom profile's or a triple's dir
/// before its lock file. A build waiting on a lock holds that file open: were it
/// unlinked, the build would lock a file no other build sees.
fn empty_target(target: &Path, locked: SystemTime, opts: &Opts) -> u64 {
    let since = locked.checked_sub(MTIME_SLACK).unwrap_or(SystemTime::UNIX_EPOCH);
    let mut bytes = 0;
    for e in read_dir(target) {
        let name = e.file_name();
        let path = e.path();
        // `DirEntry::file_type` doesn't follow symlinks.
        let dir = e.file_type().is_ok_and(|t| t.is_dir());
        let profile = name == "debug" || name == "release";
        if name == "CACHEDIR.TAG" {
            continue;
        } else if profile && !dir {
            println!("{}: not a directory, so not locked; kept", path.display());
        } else if profile {
            for f in read_dir(&path) {
                if !LOCK_FILES.iter().any(|l| f.file_name() == *l) {
                    bytes += remove(&f.path(), opts);
                }
            }
        } else if dir && has_lock_files(&path) {
            println!("{}: holds locks this run doesn't; kept", path.display());
        } else if dir && mtime_or_now(&path) >= since {
            println!("{}: modified since the locks were taken; kept", path.display());
        } else {
            bytes += remove(&path, opts);
        }
    }
    bytes
}

/// Whether `dir`, or a dir directly in it, has a Cargo lock file: a custom
/// profile (`<target>/<profile>/`) or a target triple's profile
/// (`<target>/<triple>/<profile>/`).
fn has_lock_files(dir: &Path) -> bool {
    let here = |d: &Path| LOCK_FILES.iter().any(|l| fs::symlink_metadata(d.join(l)).is_ok());
    here(dir) || read_dir(dir).iter().any(|e| e.file_type().is_ok_and(|t| t.is_dir()) && here(&e.path()))
}

fn hours(d: Duration) -> String {
    format!("{:.1} h", d.as_secs_f64() / 3600.0)
}

/// `git worktree list`: the main checkout first, then the others.
fn worktrees() -> Vec<PathBuf> {
    let out = Command::new("git").args(["worktree", "list", "--porcelain"]).output();
    let text = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    };
    text.lines().filter_map(|l| l.strip_prefix("worktree ")).map(PathBuf::from).collect()
}

/// Every other worktree's `target/` (`trees`: the main checkout first). An
/// agent worktree's is emptied once idle; the rest have their superseded
/// units removed.
fn clean_worktrees(trees: &[PathBuf], idle: Duration, opts: &Opts) -> u64 {
    let Some(main) = trees.first() else { return 0 };
    let agents = fs::canonicalize(main.join(".claude/worktrees")).ok();
    let mut bytes = 0;
    for tree in trees {
        let target = tree.join("target");
        if !target.is_dir() || opts.is_own(&target) {
            continue;
        }
        let parent = fs::canonicalize(tree).ok().and_then(|t| t.parent().map(Path::to_path_buf));
        let agent = agents.is_some() && parent == agents;
        let removed = if agent { remove_idle_target(&target, git_active(tree), idle, opts) } else { Idle::Kept };
        match removed {
            Idle::Emptied(freed) => bytes += freed,
            Idle::Locked => {}
            Idle::Kept if opts.excepts(&target) => {}
            Idle::Kept => match read_lock(&tree.join("Cargo.lock")) {
                Some(lock) => bytes += clean_target(&target, &lock, opts),
                None => println!("{}: no Cargo.lock beside it; kept", target.display()),
            },
        }
    }
    bytes
}

/// This project's Claude scratchpads: `/tmp/claude-<uid>/<slug>/`, one
/// directory per session, where the slug is the main checkout's path with
/// every character but ASCII letters and digits replaced by `-`.
fn scratch_root(main: &Path) -> PathBuf {
    PathBuf::from(format!("/tmp/claude-{}", unsafe { getuid() })).join(slug(main))
}

fn slug(path: &Path) -> String {
    path.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// Whether `dir` is a Cargo target dir: it holds a `CACHEDIR.TAG` file.
fn is_target(dir: &Path) -> bool {
    fs::symlink_metadata(dir.join("CACHEDIR.TAG")).is_ok_and(|m| m.is_file())
}

/// Cargo target dirs at most `depth` levels below `dir`; symlinks and a
/// target dir's own contents are not searched.
fn find_targets(dir: &Path, depth: usize, found: &mut Vec<PathBuf>) {
    for e in read_dir(dir) {
        let p = e.path();
        // `DirEntry::file_type` doesn't follow symlinks.
        if !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if is_target(&p) {
            found.push(p);
        } else if depth > 1 {
            find_targets(&p, depth - 1, found);
        }
    }
}

/// Idle target dirs in this project's scratchpads (reviewers build there with
/// their own `CARGO_TARGET_DIR`).
fn clean_scratch_targets(trees: &[PathBuf], idle: Duration, opts: &Opts) -> u64 {
    let Some(main) = trees.first() else { return 0 };
    let mut targets = Vec::new();
    find_targets(&scratch_root(main), 4, &mut targets);
    targets.iter().map(|t| emptied_bytes(remove_idle_target(t, SystemTime::UNIX_EPOCH, idle, opts))).sum()
}

/// Whether a target dir holds units of this workspace.
fn has_ghostrealm_units(target: &Path) -> bool {
    ["debug", "release"].iter().any(|profile| {
        read_dir(&target.join(profile).join(".fingerprint"))
            .iter()
            .any(|e| e.file_name().to_string_lossy().starts_with("ghostrealm"))
    })
}

/// The temp dir: idle ghostrealm target dirs, leftovers of tests and web
/// hosts whose process is gone, and CEF temp dirs while no host runs. Only
/// `uid`'s entries are looked at. Ages are measured to when `running` was
/// read, so what started since is never old enough.
fn clean_temp(temp: &Path, uid: u32, idle: Duration, running: &Running, opts: &Opts) -> u64 {
    let frame_dir = format!("ghostrealm-{uid}");
    let host = running.web_host();
    let (mut count, mut bytes) = (0, 0);
    for e in read_dir(temp) {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = e.path();
        // Another user's, in a shared temp dir.
        if !fs::symlink_metadata(&path).is_ok_and(|m| m.uid() == uid) {
            continue;
        }
        // The app's frame dir and its fallbacks (`ghostrealm-<uid>-<pid>-<random>`),
        // which the host clears itself, and the hosts' Chromium temp dirs.
        if name == frame_dir || name.starts_with(&format!("{frame_dir}-")) {
            if name == format!("{frame_dir}-cef") && e.file_type().is_ok_and(|t| t.is_dir()) {
                let (n, b) = clean_host_temps(&path, running, opts);
                count += n;
                bytes += b;
            }
            continue;
        }
        // A target dir's name may hold a number that isn't a pid.
        if e.file_type().is_ok_and(|t| t.is_dir()) && is_target(&path) {
            if name.starts_with("ghostrealm") || has_ghostrealm_units(&path) {
                bytes += emptied_bytes(remove_idle_target(&path, SystemTime::UNIX_EPOCH, idle, opts));
            }
            continue;
        }
        let stale = if name.starts_with("ghostrealm-") {
            leftover_is_stale(&name, &path, running)
        } else if name.starts_with(CEF_TEMP_PREFIX) {
            !host && running.older_than(mtime_or_now(&path), TEMP_MIN_AGE)
        } else {
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

/// The web hosts' Chromium temp dirs in `base` (`ghostrealm-<uid>-cef`): each
/// `<pid>` dir goes once its pid is not running and it is [`TEMP_MIN_AGE`]
/// old. Returns how many went and their bytes.
fn clean_host_temps(base: &Path, running: &Running, opts: &Opts) -> (u64, u64) {
    let (mut count, mut bytes) = (0, 0);
    for e in read_dir(base) {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        let path = e.path();
        if !running.pids.contains(&pid) && running.older_than(mtime_or_now(&path), TEMP_MIN_AGE) {
            count += 1;
            bytes += remove(&path, opts);
        }
    }
    (count, bytes)
}

/// A `ghostrealm-<tag>-<pid>[-<n>]` (or `...-<pid>.txt`) temp entry goes once
/// its pid (the first number in the name) is not running and it is
/// [`TEMP_MIN_AGE`] old. One without a number is left alone.
fn leftover_is_stale(name: &str, path: &Path, running: &Running) -> bool {
    let pid = name.split(['-', '.']).find(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()));
    let Some(pid) = pid else { return false };
    let alive = pid.parse::<u32>().map_or(true, |n| running.pids.contains(&n));
    !alive && running.older_than(mtime_or_now(path), TEMP_MIN_AGE)
}

/// An unreadable time counts as new.
fn mtime_or_now(path: &Path) -> SystemTime {
    fs::symlink_metadata(path).and_then(|m| m.modified()).unwrap_or_else(|_| SystemTime::now())
}

extern "C" {
    fn getuid() -> u32;
    #[cfg(target_os = "macos")]
    fn kill(pid: c_int, sig: c_int) -> c_int;
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

/// Remove `path` (unless dry-running). Returns the disk size removed (or
/// that would be); a failure is reported and counted in `opts`.
fn remove(path: &Path, opts: &Opts) -> u64 {
    let Ok(meta) = fs::symlink_metadata(path) else { return 0 };
    let size = disk_size(path, &meta);
    if opts.dry_run {
        if opts.verbose {
            println!("would remove {} ({})", path.display(), human(size));
        }
        return size;
    }
    match if meta.is_dir() { fs::remove_dir_all(path) } else { fs::remove_file(path) } {
        Ok(()) => {
            if opts.verbose {
                println!("removed {} ({})", path.display(), human(size));
            }
            size
        }
        Err(e) => {
            let left = fs::symlink_metadata(path).map_or(0, |m| disk_size(path, &m));
            if e.kind() != std::io::ErrorKind::NotFound {
                eprintln!("clean-stale: can't remove {}: {e}", path.display());
                opts.failures.set(opts.failures.get() + 1);
            }
            size.saturating_sub(left)
        }
    }
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
    use std::os::unix::fs::{symlink, PermissionsExt};

    /// A per-test directory under `base`, removed when dropped (also when the
    /// test fails).
    struct TmpDir(PathBuf);

    impl std::ops::Deref for TmpDir {
        type Target = Path;

        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TmpDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn tmpdir_in(base: &Path, tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let d = base.join(format!("clean-stale-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(d)
    }

    fn tmpdir(tag: &str) -> TmpDir {
        tmpdir_in(&std::env::temp_dir(), tag)
    }

    fn opts() -> Opts {
        Opts { keep: 3, ..Opts::default() }
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        File::open(path).unwrap().set_modified(t).unwrap();
    }

    /// A list read now, holding this process (running this test binary) and
    /// pid 1.
    fn running_with(programs: Vec<Exe>) -> Running {
        running_at(SystemTime::now(), programs)
    }

    fn running_at(at: SystemTime, programs: Vec<Exe>) -> Running {
        let me = (std::process::id(), Exe::At(std::env::current_exe().unwrap()));
        let others = std::iter::once(Exe::Other).chain(programs).map(|p| (1, p));
        Running::from_processes(at, std::iter::once(me).chain(others)).unwrap()
    }

    fn me() -> u32 {
        unsafe { getuid() }
    }

    /// A Cargo target dir: `CACHEDIR.TAG` and an empty `debug/`.
    fn target(dir: &Path) {
        fs::create_dir_all(dir.join("debug")).unwrap();
        fs::write(dir.join("CACHEDIR.TAG"), "Signature: 8a477f597d28d172789f06886806bc55").unwrap();
    }

    /// A target dir nothing has touched for a day, with a lock file and a
    /// build product. Only emptying uses it, which happens only on macOS.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn idle_target(dir: &Path) {
        target(dir);
        File::create(dir.join("debug/.cargo-lock")).unwrap();
        fs::create_dir(dir.join("debug/deps")).unwrap();
        fs::write(dir.join("debug/deps/app"), b"x").unwrap();
        let old = SystemTime::now() - Duration::from_secs(24 * 3600);
        for p in ["debug/.cargo-lock", "debug/deps", "debug", ""] {
            set_mtime(&dir.join(p), old);
        }
    }

    /// Whether `dir` was emptied: only its tag and lock files are left.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    fn emptied(dir: &Path) -> bool {
        dir.join("CACHEDIR.TAG").is_file()
            && dir.join("debug/.cargo-lock").is_file()
            && !dir.join("debug/deps").exists()
    }

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
        let dir = tmpdir("leftover");
        let old = SystemTime::now() - TEMP_MIN_AGE - Duration::from_secs(1);
        let entry = |name: &str, modified: SystemTime| {
            let p = dir.join(name);
            File::create(&p).unwrap().set_modified(modified).unwrap();
            p
        };
        let mut pids = running_with(vec![]);
        pids.pids.insert(4242);
        let pids = &pids;
        let dead = entry("ghostrealm-fb-1111-3", old);
        assert!(leftover_is_stale("ghostrealm-fb-1111-3", &dead, pids));
        let alive = entry("ghostrealm-fb-4242-3", old);
        assert!(!leftover_is_stale("ghostrealm-fb-4242-3", &alive, pids), "its test is running");
        let file = entry("ghostrealm-undo-4242.txt", old);
        assert!(!leftover_is_stale("ghostrealm-undo-4242.txt", &file, pids), "a pid before an extension");
        let counted = entry("ghostrealm-fb-1111-1", old);
        assert!(leftover_is_stale("ghostrealm-fb-1111-1", &counted, pids), "1 is a counter, not the pid");
        let new = entry("ghostrealm-fb-1111-4", SystemTime::now());
        assert!(!leftover_is_stale("ghostrealm-fb-1111-4", &new, pids), "too new");
    }

    #[test]
    fn the_temp_dir_keeps_frame_dirs_targets_in_use_and_cef_dirs_of_a_running_host() {
        let temp = tmpdir("temp");
        let old = SystemTime::now() - TEMP_MIN_AGE - Duration::from_secs(1);
        let uid = unsafe { getuid() };
        let dir = |name: String| {
            let p = temp.join(name);
            fs::create_dir(&p).unwrap();
            set_mtime(&p, old);
            p
        };
        let frame = dir(format!("ghostrealm-{uid}"));
        let fallback = dir(format!("ghostrealm-{uid}-1111-abc"));
        let host_temps = dir(format!("ghostrealm-{uid}-cef"));
        let gone_host = dir(format!("ghostrealm-{uid}-cef/1111"));
        let new_host = host_temps.join("2222");
        fs::create_dir(&new_host).unwrap();
        let live_host = dir(format!("ghostrealm-{uid}-cef/{}", std::process::id()));
        let leftover = dir("ghostrealm-fb-1111-3".into());
        let cef = dir(format!("{CEF_TEMP_PREFIX}1111"));
        let new_cef = temp.join(format!("{CEF_TEMP_PREFIX}2222"));
        fs::create_dir(&new_cef).unwrap();
        // Read by its name, pid 2 is gone and the dir old; built in just now.
        let built = temp.join("ghostrealm-target-2");
        target(&built);
        set_mtime(&built, old);

        let host = running_with(vec![Exe::At(PathBuf::from("/x/ghostrealm-web-host.app/Contents/MacOS/ghostrealm-web-host"))]);
        clean_temp(&temp, me() + 1, Duration::from_secs(3600), &running_with(vec![]), &opts());
        assert!(leftover.exists() && cef.exists(), "another user's temp entries are not touched");
        let an_hour_ago = SystemTime::now() - Duration::from_secs(3600);
        clean_temp(&temp, me(), Duration::from_secs(3600), &running_at(an_hour_ago, vec![]), &opts());
        assert!(leftover.exists() && cef.exists(), "too new when the process list was read");

        clean_temp(&temp, me(), Duration::from_secs(3600), &host, &opts());
        assert!(frame.is_dir() && fallback.is_dir(), "the app's frame dirs stay");
        assert!(!leftover.exists(), "its process is gone");
        assert!(cef.is_dir(), "a web host is running");
        assert!(!gone_host.exists(), "its host is gone");
        assert!(host_temps.is_dir() && live_host.is_dir(), "a running host's temp dir stays");
        assert!(new_host.is_dir(), "too new: its host may have started since");
        assert!(built.is_dir(), "a target goes by idleness, not by a number in its name");

        clean_temp(&temp, me(), Duration::from_secs(3600), &running_with(vec![]), &opts());
        assert!(!cef.exists(), "no web host runs");
        assert!(new_cef.is_dir(), "too new: its host may have started since");
        assert!(frame.is_dir() && fallback.is_dir() && built.is_dir());
    }

    // Elsewhere what runs isn't read, so nothing is emptied.
    #[cfg(target_os = "macos")]
    #[test]
    fn only_an_agent_worktrees_target_is_emptied() {
        let base = tmpdir("trees");
        let main = base.join("main");
        let agent = main.join(".claude/worktrees/agent-1");
        let owner = base.join("owner");
        let nested = main.join(".claude/worktrees/agent-1/sub");
        for tree in [&main, &agent, &owner, &nested] {
            idle_target(&tree.join("target"));
            fs::write(tree.join("Cargo.lock"), "name = \"a\"\nversion = \"1.0.0\"\n").unwrap();
        }
        let trees = [main.clone(), owner.clone(), agent.clone(), nested.clone()];
        clean_worktrees(&trees, Duration::from_secs(3600), &opts());
        assert!(main.join("target/debug/deps/app").exists(), "the main checkout's target is never emptied");
        assert!(owner.join("target/debug/deps/app").exists(), "nor that of a worktree outside .claude/worktrees");
        assert!(nested.join("target/debug/deps/app").exists(), "nor one deeper inside it");
        assert!(emptied(&agent.join("target")), "an idle agent worktree's is");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_target_dir_this_run_cleans_is_never_emptied() {
        let temp = tmpdir("own");
        let own = temp.join("ghostrealm-target");
        idle_target(&own);
        let o = Opts { own: fs::canonicalize(&own).ok(), ..opts() };
        clean_temp(&temp, me(), Duration::from_secs(3600), &running_with(vec![]), &o);
        assert!(own.join("debug/deps/app").exists());
        clean_temp(&temp, me(), Duration::from_secs(3600), &running_with(vec![]), &opts());
        assert!(emptied(&own), "another run's");
    }

    #[test]
    fn a_target_a_program_runs_from_is_in_use_however_its_path_is_spelled() {
        // /tmp is a symlink to /private/tmp on macOS.
        let dir = tmpdir_in(Path::new("/tmp"), "uses");
        let target = dir.join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        let real = fs::canonicalize(&target).unwrap();
        let app = |base: &Path| running_with(vec![Exe::At(base.join("debug/app"))]);
        assert!(app(&real).uses(&target), "run by its canonical path, asked by another");
        assert!(app(&target).uses(&real), "and the other way round");
        assert!(app(&target).uses(&target) && app(&real).uses(&real));
        let other = dir.join("other");
        fs::create_dir(&other).unwrap();
        assert!(!app(&real).uses(&other), "a sibling dir is not in use");
        assert!(running_with(vec![Exe::At("target/debug/app".into())]).uses(&other), "a relative path may be anywhere");
        assert!(running_with(vec![Exe::Unknown]).uses(&other), "an unknown path may be anywhere");
        assert!(running_with(vec![Exe::Unknown]).web_host(), "and may be a web host");
        assert!(!running_with(vec![Exe::Other]).uses(&other), "another user's process");
        assert!(app(&real).uses(&dir.join("gone")), "a dir that can't be resolved counts as in use");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn what_runs_is_read_from_the_kernel() {
        let running = Running::now().expect("the process list can be read");
        assert!(running.pids.contains(&std::process::id()));
        let exe = std::env::current_exe().unwrap();
        assert!(running.uses(exe.parent().unwrap()), "this test binary runs from its dir");
    }

    #[test]
    fn a_process_list_without_this_process_is_not_trusted() {
        let at = SystemTime::now();
        assert!(Running::from_processes(at, []).is_none(), "an empty list");
        let others = [(1, Exe::Other), (std::process::id() + 1, Exe::Other)];
        assert!(Running::from_processes(at, others).is_none(), "a list without this process");
        assert!(Running::from_processes(at, [(std::process::id(), Exe::Other)]).is_none(), "only this process");
    }

    #[test]
    fn a_process_without_a_path_is_another_users_only_if_its_owner_is_read() {
        let me = 501;
        assert!(matches!(unresolved(Owner::Uids(vec![0, 0]), me), Exe::Other));
        assert!(matches!(unresolved(Owner::Uids(vec![0, me]), me), Exe::Unknown), "set-uid by this user");
        assert!(matches!(unresolved(Owner::Uids(vec![]), me), Exe::Unknown));
        assert!(matches!(unresolved(Owner::Unknown, me), Exe::Unknown), "a sandbox may hide it");
        assert!(matches!(unresolved(Owner::Gone, me), Exe::Other));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn owners_are_read_from_the_kernel() {
        assert!(matches!(owner(std::process::id()), Owner::Uids(u) if u.contains(&me())));
        if me() != 0 {
            assert!(matches!(owner(1), Owner::Uids(u) if !u.contains(&me())), "init is root's");
        }
        // Not a spawned child: a fork while another test holds a lock would
        // hold it too, until the exec.
        let pids = list_pids().unwrap();
        let unused = (2..99_999).rev().find(|p| !pids.contains(p)).unwrap();
        assert!(matches!(owner(unused), Owner::Gone), "a pid nothing has");
    }

    #[test]
    fn target_dirs_are_found_without_following_symlinks_or_looking_inside_them() {
        let root = tmpdir("find");
        let outside = tmpdir("find-outside");
        target(&root.join("session/scratchpad/target"));
        target(&root.join("session/scratchpad/target/nested"));
        target(&outside.join("target"));
        symlink(outside.join("target"), root.join("linked-target")).unwrap();
        symlink(&*outside, root.join("linked-dir")).unwrap();
        // A tag that is a symlink doesn't make a target.
        fs::create_dir(root.join("fake")).unwrap();
        symlink(outside.join("target/CACHEDIR.TAG"), root.join("fake/CACHEDIR.TAG")).unwrap();
        let mut found = Vec::new();
        find_targets(&root, 4, &mut found);
        assert_eq!(found, vec![root.join("session/scratchpad/target")]);
    }

    #[test]
    fn the_scratchpad_slug_replaces_every_non_alphanumeric_character() {
        assert_eq!(slug(Path::new("/Users/a.b/my_repo-2")), "-Users-a-b-my-repo-2");
    }

    #[test]
    fn bad_option_values_are_errors() {
        let parse = |a: &[&str]| parse_args(a.iter().map(|s| s.to_string()));
        for bad in [
            &["--idle-hours", "nan"][..],
            &["--idle-hours", "-1"],
            &["--idle-hours", "inf"],
            &["--idle-hours", "x"],
            &["--keep", "0"],
            &["--keep", "x"],
            &["--except", "/no/such/dir/here"],
            &["--target-dir"],
            &["--bogus"],
        ] {
            assert!(parse(bad).is_err(), "{bad:?}");
        }
        let a = parse(&["--idle-hours", "0.5", "--keep", "2", "--except", "/tmp"]).unwrap();
        assert_eq!(a.idle, Duration::from_secs(1800));
        assert_eq!(a.opts.keep, 2);
        assert_eq!(a.opts.except, vec![fs::canonicalize("/tmp").unwrap()]);
    }

    #[test]
    fn a_failed_removal_is_reported_and_only_what_went_is_counted() {
        let dir = tmpdir("remove");
        let stuck = dir.join("victim/stuck");
        fs::create_dir_all(&stuck).unwrap();
        fs::write(stuck.join("f"), vec![1u8; 64 * 1024]).unwrap();
        fs::write(dir.join("victim/g"), vec![1u8; 64 * 1024]).unwrap();
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o555)).unwrap();
        let victim = dir.join("victim");
        let total = disk_size(&victim, &fs::symlink_metadata(&victim).unwrap());
        let o = opts();
        let freed = remove(&victim, &o);
        fs::set_permissions(&stuck, fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(o.failures.get(), 1);
        assert!(freed < total, "{freed} of {total}");
        assert!(stuck.join("f").exists());
    }

    #[test]
    fn emptying_keeps_the_tag_the_lock_files_and_dirs_with_other_locks() {
        let t = tmpdir("empty");
        let outside = tmpdir("empty-outside");
        target(&t);
        fs::write(t.join(".rustc_info.json"), "{}").unwrap();
        fs::create_dir_all(t.join("tmp")).unwrap();
        fs::write(t.join("tmp/x"), "x").unwrap();
        set_mtime(&t.join("tmp"), SystemTime::now() - Duration::from_secs(60));
        fs::create_dir_all(t.join("debug/deps")).unwrap();
        fs::write(outside.join("keep"), "x").unwrap();
        symlink(&*outside, t.join("release")).unwrap();
        fs::create_dir_all(t.join("profiling")).unwrap();
        File::create(t.join("profiling/.cargo-lock")).unwrap();
        fs::create_dir_all(t.join("aarch64-apple-darwin/debug/deps")).unwrap();
        File::create(t.join("aarch64-apple-darwin/debug/.cargo-build-lock")).unwrap();
        let held = BuildLock::take(&t, false).expect("no build holds it");
        assert!(t.join("debug/.cargo-artifact-lock").is_file(), "missing lock files are created");
        assert!(fs::read_dir(&*outside).unwrap().count() == 1, "not in a symlinked profile's destination");
        empty_target(&t, held.at, &opts());
        drop(held);
        let mut left: Vec<String> = Vec::new();
        for e in read_dir(&t) {
            for f in read_dir(&e.path()).into_iter().map(|f| f.path()).chain([e.path()]) {
                left.push(f.strip_prefix(&*t).unwrap().to_string_lossy().into_owned());
            }
        }
        left.sort();
        let kept = ["CACHEDIR.TAG", "debug", "aarch64-apple-darwin", "aarch64-apple-darwin/debug", "profiling"];
        let mut want: Vec<String> = kept.iter().map(|k| k.to_string()).collect();
        want.push("profiling/.cargo-lock".into());
        // Listed through the link.
        want.extend(["release".into(), "release/keep".into()]);
        want.extend(LOCK_FILES.iter().map(|l| format!("debug/{l}")));
        want.sort();
        assert_eq!(left, want, "tmp/ and .rustc_info.json go, the symlinked release stays");
        assert!(fs::symlink_metadata(t.join("release")).unwrap().is_symlink());
        assert!(outside.join("keep").exists(), "not followed");
    }

    #[test]
    fn a_target_held_by_a_build_is_not_taken() {
        let dir = tmpdir("lock");
        fs::create_dir_all(dir.join("debug")).unwrap();
        let build = File::create(dir.join("debug/.cargo-build-lock")).unwrap();
        build.lock().unwrap();
        assert!(matches!(BuildLock::take(&dir, false), Err(NotTaken::Held)), "a build holds it");
        assert!(matches!(BuildLock::take(&dir, true), Err(NotTaken::Held)), "also when dry-running");
        build.unlock().unwrap();
        let held = BuildLock::take(&dir, false).expect("free once the build is done");
        assert!(File::open(dir.join("debug/.cargo-build-lock")).unwrap().try_lock().is_err(), "held while cleaning");
        drop(held);
    }

    #[test]
    fn a_run_goes_with_its_compile_whatever_order_its_links_deps_come_in() {
        let dir = tmpdir("runs");
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
        let t = clean_profile(&dir, &lock, &opts());
        assert_eq!(t.removed, 0);
        assert!(dir.join("build/pkg-00000000000000b2").is_dir(), "the run's output stays");
    }

    /// The entries below `dir`, relative to it, sorted.
    fn listing(dir: &Path) -> Vec<String> {
        fn walk(base: &Path, dir: &Path, out: &mut Vec<String>) {
            for e in read_dir(dir) {
                out.push(e.path().strip_prefix(base).unwrap().to_string_lossy().into_owned());
                if e.file_type().is_ok_and(|t| t.is_dir()) {
                    walk(base, &e.path(), out);
                }
            }
        }
        let mut out = Vec::new();
        walk(dir, dir, &mut out);
        out.sort();
        out
    }

    #[test]
    fn a_dry_run_locks_only_the_lock_files_there_are_and_creates_nothing() {
        let t = tmpdir("dry");
        target(&t);
        File::create(t.join("debug/.cargo-lock")).unwrap();
        let before = listing(&t);
        let held = BuildLock::take(&t, true).expect("no build holds it");
        assert_eq!(listing(&t), before, "no profile dir or lock file is created");
        assert!(File::open(t.join("debug/.cargo-lock")).unwrap().try_lock().is_err(), "the one there is held");
        drop(held);
        let o = Opts { dry_run: true, ..opts() };
        clean_target(&t, &Lock::default(), &o);
        assert_eq!(listing(&t), before, "nor by a dry-run clean");
    }

    #[test]
    fn a_target_that_cant_be_written_is_not_reported_as_held() {
        let t = tmpdir("readonly");
        target(&t);
        File::create(t.join("debug/.cargo-lock")).unwrap();
        fs::create_dir(t.join("release")).unwrap();
        let read_only = |mode| {
            for d in ["debug", "release", ""] {
                fs::set_permissions(t.join(d), fs::Permissions::from_mode(mode)).unwrap();
            }
        };
        read_only(0o555);
        let wet = BuildLock::take(&t, false).map(|_| ());
        let dry = BuildLock::take(&t, true).map(|_| ());
        read_only(0o755);
        assert!(
            matches!(&wet, Err(NotTaken::Unopenable(p, _)) if p.parent() == Some(&*t.join("debug"))),
            "a missing lock file it can't create: {wet:?}"
        );
        assert!(dry.is_ok(), "a dry run creates nothing, so needs no write access: {dry:?}");

        // Lock files that exist are opened read-only.
        BuildLock::take(&t, false).expect("creates them while writable");
        read_only(0o555);
        for l in LOCK_FILES {
            fs::set_permissions(t.join("debug").join(l), fs::Permissions::from_mode(0o444)).unwrap();
        }
        let taken = BuildLock::take(&t, false).map(|_| ());
        read_only(0o755);
        assert!(taken.is_ok(), "{taken:?}");
    }

    #[test]
    fn a_symlinked_profile_is_neither_locked_nor_cleaned() {
        let t = tmpdir("symprofile");
        let outside = tmpdir("symprofile-outside");
        target(&t);
        // More incremental dirs of one crate than are kept.
        for i in 0..KEEP_INCREMENTAL + 2 {
            let d = outside.join(format!("incremental/foo-{i}"));
            fs::create_dir_all(&d).unwrap();
            fs::write(d.join("x"), vec![1u8; 4096]).unwrap();
        }
        // A build in the destination's own target holds its lock.
        let build = File::create(outside.join(".cargo-lock")).unwrap();
        build.lock().unwrap();
        symlink(&*outside, t.join("release")).unwrap();
        let before = listing(&outside);
        let held = BuildLock::take(&t, false).expect("the destination's lock isn't this target's");
        assert_eq!(listing(&outside), before, "no lock file is created there");
        drop(held);
        clean_target(&t, &Lock::default(), &opts());
        assert_eq!(listing(&outside), before, "nothing there is removed");

        // The same in a real profile is cleaned, and counted with no units.
        let real = t.join("debug/incremental");
        fs::rename(outside.join("incremental"), &real).unwrap();
        let freed = clean_target(&t, &Lock::default(), &opts());
        assert_eq!(read_dir(&real).len(), KEEP_INCREMENTAL);
        assert!(freed > 0, "incremental dirs count without units");
    }

    #[test]
    fn emptying_keeps_a_dir_modified_since_the_locks_were_taken() {
        let t = tmpdir("race");
        target(&t);
        fs::create_dir(t.join("old")).unwrap();
        set_mtime(&t.join("old"), SystemTime::now() - Duration::from_secs(60));
        let held = BuildLock::take(&t, false).expect("no build holds it");
        // A build creating a triple's dir, its lock file not yet.
        fs::create_dir_all(t.join("x86_64-apple-darwin/debug")).unwrap();
        empty_target(&t, held.at, &opts());
        assert!(!t.join("old").exists());
        assert!(t.join("x86_64-apple-darwin/debug").is_dir());
    }

    #[test]
    fn a_lock_file_that_is_a_dangling_symlink_is_not_created_through() {
        let t = tmpdir("dangling");
        let outside = tmpdir("dangling-outside");
        target(&t);
        symlink(outside.join("lock"), t.join("debug/.cargo-lock")).unwrap();
        let taken = BuildLock::take(&t, false).map(|_| ());
        assert!(
            matches!(&taken, Err(NotTaken::Unopenable(p, _)) if *p == t.join("debug/.cargo-lock")),
            "{taken:?}"
        );
        assert!(!outside.join("lock").exists(), "its destination isn't created");
    }

    #[test]
    fn a_dry_run_emptying_leaves_the_tree_unchanged() {
        let base = tmpdir("dry-empty");
        let t = base.join("ghostrealm-target");
        idle_target(&t);
        fs::create_dir_all(t.join("tmp")).unwrap();
        fs::write(t.join(".rustc_info.json"), "{}").unwrap();
        for p in ["tmp", ".rustc_info.json"] {
            set_mtime(&t.join(p), SystemTime::now() - Duration::from_secs(24 * 3600));
        }
        let before = listing(&t);
        let o = Opts { dry_run: true, ..opts() };
        let held = BuildLock::take(&t, true).expect("no build holds it");
        assert!(empty_target(&t, held.at, &o) > 0, "it would free something");
        drop(held);
        assert_eq!(listing(&t), before, "empty_target");
        let idle = remove_idle_target(&t, SystemTime::UNIX_EPOCH, Duration::from_secs(3600), &o);
        // What runs is read only on macOS; elsewhere it is kept before locking.
        if cfg!(target_os = "macos") {
            assert!(matches!(idle, Idle::Emptied(b) if b > 0), "reached emptying: {idle:?}");
        } else {
            assert_eq!(idle, Idle::Kept);
        }
        assert_eq!(listing(&t), before, "remove_idle_target");
    }
}
