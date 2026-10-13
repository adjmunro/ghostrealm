//! Removes build artifacts under `target/{debug,release}` that a newer build of
//! the same unit superseded. Cargo never removes them: every change of
//! toolchain, dependency version, feature set, or profile leaves the old
//! hashed copies in `deps/`, `build/`, `.fingerprint/` and `incremental/`.
//!
//! A unit (one `.fingerprint/<package>-<hash>` directory) is identified by what
//! its fingerprint records: target kind (`lib`, `test-bin`, `check-lib`,
//! `run-build-script`, ...), features, target hash, rustflags, and the source
//! file it was compiled from (which tells package versions apart). Of the units
//! with the same identity the `--keep` most recently built (default 3) stay and
//! the rest are removed, with their `deps/` files (`<name>-<hash>*`) and
//! `build/<package>-<hash>/`. Profile hashes are not part of the identity, so a
//! profile change supersedes the old copies. Several stay because
//! `cargo build`, `cargo test --workspace`, `cargo clippy` and `-p` builds
//! resolve features differently and so produce sibling copies of the same
//! unit that are all in use; the number kept bounds the stale ones. A unit
//! reused by the latest build but older than `--keep` newer siblings is
//! removed too; it is rebuilt on the next build. A unit built from a registry
//! package version that `Cargo.lock` no longer lists is removed outright. Units whose fingerprint can't be read are left alone.
//! `incremental/` has no such record, so the newest few directories per crate
//! are kept (it is a cache; a lost directory only costs one slower compile).
//!
//! Only `target/` is touched; `~/.cargo` is never read or written. Do not run
//! it while a build is in progress.
//!
//! Run it from the repo root with `mise run clean-stale [-- --dry-run]`, or
//! build it with `rustc -O --edition 2021 scripts/clean-stale.rs`. Options:
//! `--dry-run`/`-n` (remove nothing), `--verbose`/`-v` (list each path),
//! `--keep N`, `--target-dir DIR`, `--lockfile FILE`.

use std::collections::HashMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::SystemTime;

/// Incremental-compilation directories kept per crate name (lib, bin, and
/// their test builds each have one).
const KEEP_INCREMENTAL: usize = 4;

struct Unit {
    package: String,
    hash: String,
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
}

fn main() -> ExitCode {
    let mut opts = Opts { dry_run: false, verbose: false, keep: 3 };
    let mut target = PathBuf::from("target");
    let mut lockfile = PathBuf::from("Cargo.lock");
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--dry-run" | "-n" => opts.dry_run = true,
            "--verbose" | "-v" => opts.verbose = true,
            "--keep" => opts.keep = args.next().and_then(|n| n.parse().ok()).unwrap_or(opts.keep).max(1),
            "--target-dir" => target = args.next().map(PathBuf::from).unwrap_or(target),
            "--lockfile" => lockfile = args.next().map(PathBuf::from).unwrap_or(lockfile),
            other => {
                eprintln!("clean-stale: unknown argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let lock = read_lock(&lockfile);
    let mut total = Totals::default();
    for profile in ["debug", "release"] {
        let dir = target.join(profile);
        if dir.is_dir() {
            let t = clean_profile(&dir, &lock, &opts);
            println!(
                "{}: {} of {} units superseded; {} {}",
                dir.display(),
                t.removed,
                t.units,
                if opts.dry_run { "would free" } else { "freed" },
                human(t.bytes)
            );
            total.bytes += t.bytes;
        }
    }
    println!("{} {}", if opts.dry_run { "would free" } else { "freed" }, human(total.bytes));
    ExitCode::SUCCESS
}

/// What `Cargo.lock` lists.
#[derive(Default)]
struct Lock {
    /// `<name>-<version>` of each package, as registry source directories are named.
    dirs: std::collections::HashSet<String>,
}

fn read_lock(lockfile: &Path) -> Lock {
    let mut lock = Lock::default();
    let text = fs::read_to_string(lockfile).unwrap_or_default();
    let mut name = None;
    for line in text.lines() {
        let quoted = |key: &str| line.strip_prefix(key).and_then(|n| n.strip_suffix('"')).map(str::to_string);
        if let Some(n) = quoted("name = \"") {
            name = Some(n);
        } else if let (Some(v), Some(n)) = (quoted("version = \""), name.take()) {
            lock.dirs.insert(format!("{n}-{v}"));
        }
    }
    lock
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

    // identity -> units sharing it.
    let mut groups: HashMap<String, (usize, Vec<Unit>)> = HashMap::new();
    for e in read_dir(&dir.join(".fingerprint")) {
        let Some(dir_name) = e.file_name().to_str().map(str::to_string) else { continue };
        let Some(package) = dir_name.len().checked_sub(17).and_then(|n| dir_name.get(..n)) else { continue };
        let Some((_, hash)) = split_hash(&dir_name) else { continue };
        let Some((kind, json, built)) = fingerprint_json(&e.path()) else { continue };
        let source = dep_info_source(by_hash.get(hash), &dir.join("build").join(&dir_name));
        let identity = format!(
            "{package}|{kind}|{}|{}|{}|{}|{source}",
            field(&json, "features"),
            field(&json, "target"),
            field(&json, "compile_kind"),
            field(&json, "rustflags"),
        );
        let keep = if unlisted_registry_source(&source, lock) { 0 } else { opts.keep };
        let unit = Unit { package: package.to_string(), hash: hash.to_string(), built };
        groups.entry(identity).or_insert((keep, Vec::new())).1.push(unit);
    }

    for (keep, mut units) in groups.into_values() {
        totals.units += units.len();
        units.sort_by(|a, b| b.built.cmp(&a.built).then_with(|| a.hash.cmp(&b.hash)));
        for unit in units.into_iter().skip(keep) {
            let name = format!("{}-{}", unit.package, unit.hash);
            let mut paths = vec![dir.join(".fingerprint").join(&name), dir.join("build").join(&name)];
            paths.extend(by_hash.get(&unit.hash).cloned().unwrap_or_default());
            totals.removed += 1;
            for p in paths {
                totals.bytes += remove(&p, opts);
            }
        }
    }

    // incremental/<crate>-<id>: newest few per crate.
    let mut crates: HashMap<String, Vec<(SystemTime, PathBuf)>> = HashMap::new();
    for e in read_dir(&dir.join("incremental")) {
        let name = e.file_name().to_string_lossy().into_owned();
        let Some((krate, _)) = name.rsplit_once('-') else { continue };
        let modified = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        crates.entry(krate.to_string()).or_default().push((modified, e.path()));
    }
    for mut dirs in crates.into_values() {
        dirs.sort_by(|a, b| b.0.cmp(&a.0));
        for (_, p) in dirs.into_iter().skip(KEEP_INCREMENTAL) {
            totals.bytes += remove(&p, opts);
        }
    }
    totals
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
