//! The app build script's web host rules (`build/web_host.rs`): skips, dirs,
//! the nested build's arguments and environment, rerun triggers, the stamp,
//! and placement, whose guards keep any bundle they can't recognise.

#[allow(dead_code)]
#[path = "../build/web_host.rs"]
mod web_host;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use web_host::*;

/// A per-test directory, removed when dropped (permissions restored first).
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> TmpDir {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let d = std::env::temp_dir().join(format!("ghostrealm-webbuild-{tag}-{}-{n}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        TmpDir(fs::canonicalize(d).unwrap())
    }
}

impl std::ops::Deref for TmpDir {
    type Target = Path;
    fn deref(&self) -> &Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::process::Command::new("chmod").arg("-R").arg("u+rwx").arg(&self.0).status();
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn the_bundle_id_is_the_one_the_bundler_writes() {
    assert_eq!(BUNDLE_ID, ghostrealm_web::temp::BUNDLE_ID);
    assert_eq!(HOST, ghostrealm_web::client::HOST_NAME);
}

#[test]
fn no_web_is_off_only_when_unset_empty_or_zero() {
    assert!(!no_web(None));
    assert!(!no_web(Some("")));
    assert!(!no_web(Some("0")));
    assert!(no_web(Some("1")));
    assert!(no_web(Some("yes")));
}

fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: BTreeMap<String, String> = pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    move |k| map.get(k).cloned()
}

#[test]
fn skips_name_their_reason() {
    let mac = [("CARGO_CFG_TARGET_OS", "macos"), ("TARGET", "aarch64-apple-darwin"), ("HOST", "aarch64-apple-darwin")];
    assert_eq!(env_skip(env_of(&mac)), None);
    let with = |extra: (&'static str, &'static str)| {
        let mut v = mac.to_vec();
        v.retain(|(k, _)| *k != extra.0);
        v.push(extra);
        env_skip(env_of(&v))
    };
    assert!(with(("GHOSTREALM_NO_WEB", "1")).unwrap().contains("GHOSTREALM_NO_WEB"));
    assert_eq!(with(("GHOSTREALM_NO_WEB", "0")), None);
    assert!(with(("RA_RUSTC_WRAPPER", "1")).unwrap().contains("rust-analyzer"));
    assert!(with(("CARGO_CFG_TARGET_OS", "linux")).unwrap().contains("linux"));
    assert!(with(("TARGET", "x86_64-apple-darwin")).unwrap().contains("cross-compiling"));
}

/// A build dir holding `OUT_DIR` at `rel`, and its target dir's profile dir.
fn layout(base: &Path, rel: &str, artifact: &str) -> PathBuf {
    let out = base.join("build-dir").join(rel);
    fs::create_dir_all(&out).unwrap();
    fs::create_dir_all(base.join("target").join(artifact)).unwrap();
    out
}

const UNIT: &str = "ghostrealm-0123456789abcdef";

#[test]
fn dirs_follow_the_out_dir_shape() {
    let t = TmpDir::new("dirs");
    let out = layout(&t, &format!("debug/build/{UNIT}/out"), "debug");
    let d = dirs(&out, &t.join("target"), Some(&t.join("build-dir")), "aarch64-apple-darwin").unwrap();
    assert_eq!(d.artifact, t.join("target/debug"));
    assert_eq!(d.nested, t.join("target/web-host"));
    assert_eq!((d.triple.as_deref(), d.profile(), d.profile_dir.as_str()), (None, "dev", "debug"));
    assert_eq!(d.nested_profile(), t.join("target/web-host/debug"));

    let out = layout(&t, &format!("aarch64-apple-darwin/release/build/{UNIT}/out"), "aarch64-apple-darwin/release");
    let d = dirs(&out, &t.join("target"), Some(&t.join("build-dir")), "aarch64-apple-darwin").unwrap();
    assert_eq!(d.artifact, t.join("target/aarch64-apple-darwin/release"));
    assert_eq!((d.triple.as_deref(), d.profile()), (Some("aarch64-apple-darwin"), "release"));
    assert_eq!(d.nested_profile(), t.join("target/web-host/aarch64-apple-darwin/release"));

    let out = layout(&t, &format!("profiling/build/{UNIT}/out"), "profiling");
    let d = dirs(&out, &t.join("target"), None, "x").unwrap_err();
    assert!(matches!(d, DirsError::Layout(_)), "no build dir: the target dir is the build dir: {d:?}");
    let d = dirs(&out, &t.join("build-dir"), None, "x").unwrap();
    assert_eq!((d.profile(), d.artifact.clone()), ("profiling", t.join("build-dir/profiling")));

    let out = t.join("build-dir/custom/build").join(UNIT).join("out");
    fs::create_dir_all(&out).unwrap();
    let d = dirs(&out, &t.join("target"), Some(&t.join("build-dir")), "x");
    assert!(matches!(d, Err(DirsError::Fail(_))), "the target dir has no such profile dir: {d:?}");
}

#[test]
fn an_out_dir_of_another_shape_or_elsewhere_is_a_layout_skip() {
    let t = TmpDir::new("shape");
    let build = t.join("build-dir");
    let target = t.join("target");
    fs::create_dir_all(&build).unwrap();
    let skip = |out: &Path| match dirs(out, &target, Some(&build), "aarch64-apple-darwin") {
        Err(DirsError::Layout(why)) => assert!(why.contains("CARGO_TARGET_DIR"), "{why}"),
        other => panic!("{}: {other:?}", out.display()),
    };
    // `cargo build --target-dir X`: metadata doesn't see X.
    let elsewhere = t.join("other/debug/build").join(UNIT).join("out");
    fs::create_dir_all(&elsewhere).unwrap();
    fs::create_dir_all(target.join("debug")).unwrap();
    skip(&elsewhere);
    skip(&layout(&t, &format!("x86_64-apple-darwin/debug/build/{UNIT}/out"), "debug"));
    skip(&layout(&t, &format!("debug/build/{UNIT}/out/more"), "debug"));
    skip(&layout(&t, "debug/build/other-0123456789abcdef/out", "debug"));
    skip(&layout(&t, "build/ghostrealm/0123456789abcdef/out", "debug"));
    assert!(!target.join("web-host").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_artifact_dir_fails() {
    let t = TmpDir::new("artifact-link");
    let out = layout(&t, &format!("debug/build/{UNIT}/out"), "elsewhere");
    std::os::unix::fs::symlink(t.join("target/elsewhere"), t.join("target/debug")).unwrap();
    let d = dirs(&out, &t.join("target"), Some(&t.join("build-dir")), "x");
    assert!(matches!(d, Err(DirsError::Fail(_))), "{d:?}");
}

#[test]
fn the_nested_build_has_its_own_target_and_build_dir() {
    let d = Dirs {
        target: "/t".into(),
        artifact: "/t/aarch64-apple-darwin/debug".into(),
        nested: "/t/web-host".into(),
        triple: Some("aarch64-apple-darwin".into()),
        profile_dir: "debug".into(),
    };
    let args: Vec<String> = nested_args(&d).into_iter().map(|a| a.into_string().unwrap()).collect();
    let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].as_str());
    assert_eq!(after("--target-dir"), Some("/t/web-host"));
    assert_eq!(after("--config"), Some("build.build-dir=\"/t/web-host\""));
    assert_eq!(after("--target"), Some("aarch64-apple-darwin"));
    assert_eq!(after("--profile"), Some("dev"));
    assert_eq!(after("-p"), Some(HOST));
    assert!(args.contains(&"--locked".into()) && args.contains(&"--bins".into()));
    let none = Dirs { triple: None, profile_dir: "release".into(), ..d };
    let args: Vec<String> = nested_args(&none).into_iter().map(|a| a.into_string().unwrap()).collect();
    assert!(!args.contains(&"--target".into()));
    assert!(args.windows(2).any(|w| w == ["--profile", "release"]));
}

#[test]
fn the_env_filter_drops_what_cargo_set_for_this_script() {
    for drop in [
        "OUT_DIR",
        "TARGET",
        "HOST",
        "PROFILE",
        "OPT_LEVEL",
        "DEBUG",
        "NUM_JOBS",
        "RUSTC_LINKER",
        "CARGO_PKG_NAME",
        "CARGO_MANIFEST_DIR",
        "CARGO_MANIFEST_LINKS",
        "CARGO_CFG_TARGET_OS",
        "CARGO_FEATURE_DEFAULT",
        "DEP_Z_INCLUDE",
        "CARGO_PRIMARY_PACKAGE",
        "CARGO_ENCODED_RUSTFLAGS",
        "DYLD_FALLBACK_LIBRARY_PATH",
        "RUSTC_WRAPPER",
        "RUSTC_WORKSPACE_WRAPPER",
    ] {
        assert!(!keep_env(drop), "{drop}");
    }
    for keep in [
        "PATH",
        "HOME",
        "CARGO",
        "CARGO_MAKEFLAGS",
        "RUSTFLAGS",
        "CARGO_HOME",
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "CARGO_NET_OFFLINE",
        "CEF_PATH",
        "MACOSX_DEPLOYMENT_TARGET",
    ] {
        assert!(keep_env(keep), "{keep}");
    }
}

#[test]
fn nested_env_triggers_are_read_in_both_spellings() {
    let output = "cargo:rustc-link-lib=z\n\
                  cargo:rerun-if-env-changed=CC\n\
                  cargo::rerun-if-env-changed=CEF_PATH\n\
                  cargo:rerun-if-env-changed=TARGET\n\
                  cargo::rerun-if-env-changed=CARGO_CFG_TARGET_OS\n\
                  cargo:rerun-if-changed=build.rs\n";
    assert_eq!(env_triggers(output), ["CC", "CEF_PATH"]);
}

#[test]
fn json_messages_give_the_executables_and_out_dirs() {
    let mut b = Built::default();
    for line in [
        r#"{"reason":"compiler-artifact","target":{"name":"ghostrealm-web-host"},"executable":"/n/debug/ghostrealm-web-host","fresh":true}"#,
        r#"{"reason":"compiler-artifact","target":{"name":"ghostrealm-web-host-helper"},"executable":"/n/debug/h"}"#,
        r#"{"reason":"compiler-artifact","target":{"name":"ghostrealm-web-bundle"},"executable":"/n/debug/b"}"#,
        r#"{"reason":"compiler-artifact","target":{"name":"cef"},"executable":null}"#,
        r#"{"reason":"build-script-executed","package_id":"x","out_dir":"/n/debug/build/cc-1/out"}"#,
        r#"{"reason":"build-finished","success":true}"#,
        "not json",
    ] {
        b.read(line);
    }
    assert_eq!(b.host.as_deref(), Some(Path::new("/n/debug/ghostrealm-web-host")));
    assert_eq!(b.helper.as_deref(), Some(Path::new("/n/debug/h")));
    assert_eq!(b.bundler.as_deref(), Some(Path::new("/n/debug/b")));
    assert_eq!(b.out_dirs, [PathBuf::from("/n/debug/build/cc-1/out")]);
}

#[test]
fn metadata_without_a_build_directory_has_none() {
    let (t, b) = metadata_dirs(r#"{"target_directory":"/t","build_directory":"/b"}"#).unwrap();
    assert_eq!((t, b), (PathBuf::from("/t"), Some(PathBuf::from("/b"))));
    let (_, b) = metadata_dirs(r#"{"target_directory":"/t"}"#).unwrap();
    assert_eq!(b, None);
    assert!(metadata_dirs("{}").is_err());
    assert!(metadata_dirs("").is_err());
}

#[test]
fn rerun_paths_are_only_those_that_exist() {
    let t = TmpDir::new("rerun");
    fs::create_dir_all(t.join("crates/web-host/src")).unwrap();
    fs::write(t.join("crates/web-host/Cargo.toml"), "").unwrap();
    fs::write(t.join("Cargo.lock"), "").unwrap();
    let got = rerun_paths(&t);
    assert_eq!(got, [t.join("crates/web-host/src"), t.join("crates/web-host/Cargo.toml"), t.join("Cargo.lock")]);
}

fn cargo() -> String {
    std::env::var("CARGO").unwrap_or_else(|_| "cargo".into())
}

fn workspace() -> PathBuf {
    fs::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap()
}

/// Every lib, bin and build-script source and every manifest of the host's
/// path-dependency closure is watched, so a change to any reruns the script.
#[test]
fn the_rerun_paths_cover_the_hosts_path_dependency_closure() {
    let root = workspace();
    let out = std::process::Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&root)
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let meta: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let packages = meta["packages"].as_array().unwrap();
    let by_dir = |dir: &Path| {
        packages.iter().find(|p| Path::new(p["manifest_path"].as_str().unwrap()).parent() == Some(dir))
    };
    let host = packages.iter().find(|p| p["name"] == HOST).unwrap();
    let mut todo = vec![host];
    let mut seen = Vec::new();
    while let Some(p) = todo.pop() {
        if seen.contains(&p["name"]) {
            continue;
        }
        seen.push(p["name"].clone());
        for d in p["dependencies"].as_array().unwrap() {
            if d["kind"] == "dev" {
                continue;
            }
            if let Some(path) = d["path"].as_str() {
                todo.push(by_dir(Path::new(path)).unwrap_or_else(|| panic!("no package at {path}")));
            }
        }
        let watched = rerun_paths(&root);
        let manifest = PathBuf::from(p["manifest_path"].as_str().unwrap());
        assert!(watched.contains(&manifest), "{} not watched", manifest.display());
        for t in p["targets"].as_array().unwrap() {
            let kinds: Vec<&str> = t["kind"].as_array().unwrap().iter().filter_map(|k| k.as_str()).collect();
            if !kinds.iter().any(|k| ["lib", "rlib", "bin", "custom-build", "proc-macro"].contains(k)) {
                continue;
            }
            let src = PathBuf::from(t["src_path"].as_str().unwrap());
            assert!(watched.iter().any(|w| src.starts_with(w)), "{} not watched", src.display());
        }
    }
    assert!(seen.len() >= 3, "{seen:?}");
}

#[cfg(target_os = "macos")]
mod placement {
    use super::*;
    use std::time::{Duration, SystemTime};

    fn uid(p: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        fs::metadata(p).unwrap().uid()
    }

    /// Every file and symlink under `dir` (a symlink's target, not followed),
    /// with its bytes.
    fn snapshot(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
        let mut all = BTreeMap::new();
        let mut todo = vec![dir.to_path_buf()];
        while let Some(d) = todo.pop() {
            let m = fs::symlink_metadata(&d).unwrap();
            if m.file_type().is_symlink() {
                all.insert(d.clone(), fs::read_link(&d).unwrap().into_os_string().into_encoded_bytes());
            } else if m.is_dir() {
                all.insert(d.clone(), b"/".to_vec());
                for e in fs::read_dir(&d).unwrap() {
                    todo.push(e.unwrap().path());
                }
            } else {
                all.insert(d.clone(), fs::read(&d).unwrap_or_else(|_| b"<unreadable>".to_vec()));
            }
        }
        all
    }

    fn plist(id: &str) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
             <plist version=\"1.0\">\n<dict>\n<key>CFBundleIdentifier</key>\n<string>{id}</string>\n\
             <key>CFBundleExecutable</key>\n<string>{HOST}</string>\n</dict>\n</plist>\n"
        )
    }

    /// A bundle at `app` with every required file (holding `tag`) and a plist
    /// naming `id`.
    fn fake_app(app: &Path, id: &str, tag: &str) {
        for f in required_files() {
            let p = app.join(f);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, tag).unwrap();
        }
        fs::write(app.join("Contents/Info.plist"), plist(id)).unwrap();
    }

    /// An artifact dir and a staged bundle (`new`) beside it.
    struct Setup {
        t: TmpDir,
        artifact: PathBuf,
        staged: PathBuf,
        uid: u32,
    }

    impl Setup {
        fn new(tag: &str) -> Setup {
            let t = TmpDir::new(tag);
            let artifact = t.join("target/debug");
            fs::create_dir_all(&artifact).unwrap();
            let staged = t.join("target/web-host/debug").join(APP);
            fake_app(&staged, BUNDLE_ID, "new");
            let uid = uid(&t);
            Setup { t, artifact, staged, uid }
        }

        fn dest(&self) -> PathBuf {
            self.artifact.join(APP)
        }

        fn old(&self) -> PathBuf {
            self.artifact.join(OLD)
        }

        fn place(&self) -> Result<Vec<String>, String> {
            place(&self.staged, &self.artifact, self.uid)
        }

        /// `place` fails and the artifact dir (and `also`) is unchanged.
        fn refused(&self, also: &[&Path]) -> String {
            let before: Vec<_> = std::iter::once(self.artifact.as_path()).chain(also.iter().copied()).map(snapshot).collect();
            let err = self.place().unwrap_err();
            let after: Vec<_> = std::iter::once(self.artifact.as_path()).chain(also.iter().copied()).map(snapshot).collect();
            assert_eq!(before, after, "kept byte for byte ({err})");
            err
        }

        fn new_in_place(&self) {
            assert_eq!(fs::read_to_string(self.dest().join("Contents/MacOS").join(HOST)).unwrap(), "new");
            assert!(!self.old().exists(), ".old removed");
            assert!(!self.staged.exists(), "staged moved");
        }
    }

    #[test]
    fn a_symlinked_dest_and_its_target_are_kept() {
        let s = Setup::new("dest-link");
        let elsewhere = s.t.join("Real.app");
        fake_app(&elsewhere, BUNDLE_ID, "real");
        std::os::unix::fs::symlink(&elsewhere, s.dest()).unwrap();
        let err = s.refused(&[&elsewhere]);
        assert!(err.contains("not a directory"), "{err}");
    }

    #[test]
    fn a_dest_that_is_a_file_is_kept() {
        let s = Setup::new("dest-file");
        fs::write(s.dest(), "a file").unwrap();
        s.refused(&[]);
    }

    #[test]
    fn a_dest_without_a_readable_plist_naming_the_bundle_id_is_kept() {
        type Spoil = fn(&Path);
        let cases: [(&str, Spoil); 6] = [
            ("missing", |p| fs::remove_file(p).unwrap()),
            ("empty", |p| fs::write(p, "").unwrap()),
            ("truncated", |p| {
                let text = plist(BUNDLE_ID);
                fs::write(p, &text[..text.find("<string>").unwrap() + 12]).unwrap()
            }),
            ("unreadable", |p| {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(p, fs::Permissions::from_mode(0o000)).unwrap()
            }),
            ("other id", |p| fs::write(p, plist("com.example.other")).unwrap()),
            ("id prefix", |p| fs::write(p, plist(&format!("{BUNDLE_ID}.x"))).unwrap()),
        ];
        for (name, spoil) in cases {
            let s = Setup::new("dest-plist");
            fake_app(&s.dest(), BUNDLE_ID, "existing");
            spoil(&s.dest().join("Contents/Info.plist"));
            let err = s.refused(&[]);
            assert!(err.contains("Info.plist"), "{name}: {err}");
        }
    }

    #[test]
    fn a_dest_of_another_user_is_not_ours() {
        let s = Setup::new("dest-uid");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        assert_eq!(ours(&s.dest(), s.uid), Ok(()));
        let err = ours(&s.dest(), s.uid + 1).unwrap_err();
        assert!(err.contains("owned by uid"), "{err}");
    }

    #[test]
    fn an_incomplete_staged_bundle_touches_nothing() {
        let s = Setup::new("staged-incomplete");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        fs::remove_dir_all(s.staged.join("Contents/Frameworks/Chromium Embedded Framework.framework")).unwrap();
        let err = s.refused(&[]);
        assert!(err.contains("Chromium Embedded Framework"), "{err}");
        let s = Setup::new("staged-empty");
        fs::write(s.staged.join("Contents/MacOS").join(HOST), "").unwrap();
        s.refused(&[]);
        let s = Setup::new("staged-not-ours");
        fs::write(s.staged.join("Contents/Info.plist"), plist("com.example.other")).unwrap();
        s.refused(&[]);
    }

    #[test]
    fn a_failed_second_rename_restores_dest() {
        use std::os::unix::fs::PermissionsExt;
        let s = Setup::new("rename");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        let before = snapshot(&s.artifact);
        let stage_dir = s.staged.parent().unwrap();
        fs::set_permissions(stage_dir, fs::Permissions::from_mode(0o555)).unwrap();
        let err = s.place().unwrap_err();
        fs::set_permissions(stage_dir, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("rename"), "{err}");
        assert_eq!(snapshot(&s.artifact), before, "dest restored, no .old left");
    }

    #[test]
    fn an_old_that_is_not_ours_is_kept() {
        let s = Setup::new("old-foreign");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        fake_app(&s.old(), "com.example.other", "foreign");
        let err = s.refused(&[]);
        assert!(err.contains("remove it by hand"), "{err}");
        // With no dest, a foreign .old isn't restored either.
        fs::remove_dir_all(s.dest()).unwrap();
        s.refused(&[]);
    }

    #[test]
    fn with_no_dest_the_staged_bundle_is_placed() {
        let s = Setup::new("fresh");
        assert_eq!(s.place(), Ok(vec![]));
        s.new_in_place();
    }

    #[test]
    fn a_dest_that_is_ours_is_replaced() {
        let s = Setup::new("replace");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        assert_eq!(s.place(), Ok(vec![]));
        s.new_in_place();
    }

    #[test]
    fn an_old_of_ours_with_no_dest_is_restored_then_replaced() {
        let s = Setup::new("restore");
        fake_app(&s.old(), BUNDLE_ID, "killed run");
        assert_eq!(s.place(), Ok(vec![]));
        s.new_in_place();
    }

    #[test]
    fn a_leftover_old_of_ours_beside_dest_is_removed() {
        let s = Setup::new("old-ours");
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        fake_app(&s.old(), BUNDLE_ID, "leftover");
        assert_eq!(s.place(), Ok(vec![]));
        s.new_in_place();
    }

    /// Host, helper and bundler executables with mtimes in the past.
    fn exes(dir: &Path) -> [PathBuf; 3] {
        fs::create_dir_all(dir).unwrap();
        let old = SystemTime::now() - Duration::from_secs(3600);
        [HOST, HELPER, BUNDLER].map(|n| {
            let p = dir.join(n);
            fs::write(&p, n).unwrap();
            fs::File::open(&p).unwrap().set_modified(old).unwrap();
            p
        })
    }

    /// The bundler's job: a staged bundle whose host executable is a copy
    /// (`fs::copy`, as CEF's bundler makes it) of the nested one.
    fn bundle_from(s: &Setup, host: &Path) -> Result<PathBuf, String> {
        fake_app(&s.staged, BUNDLE_ID, "new");
        let _ = fs::remove_file(s.staged.join(STAMP));
        fs::copy(host, s.staged.join("Contents/MacOS").join(HOST)).unwrap();
        Ok(s.staged.clone())
    }

    fn mtime(p: &Path) -> SystemTime {
        fs::metadata(p).unwrap().modified().unwrap()
    }

    #[test]
    fn install_bundles_places_and_settles_the_placed_executable() {
        let s = Setup::new("install");
        fs::remove_dir_all(&s.staged).unwrap();
        let [host, helper, bundler] = exes(&s.t.join("target/web-host/debug"));
        // Linked during this run: newer than its start.
        fs::File::open(&host).unwrap().set_modified(SystemTime::now()).unwrap();
        let (host_m, helper_m) = (mtime(&host), mtime(&helper));
        let st = stamp(&host, &helper, &bundler).unwrap();
        let start = SystemTime::now();
        let done = install(&s.artifact, &st, s.uid, start, || bundle_from(&s, &host)).unwrap();
        assert!(done.placed && done.warnings.is_empty(), "{done:?}");
        let exe = s.dest().join("Contents/MacOS").join(HOST);
        assert_eq!(done.trigger.as_deref(), Some(exe.as_path()));
        assert!(mtime(&exe) <= start - SETTLE, "settled before the run's start");
        assert_eq!((mtime(&host), mtime(&helper)), (host_m, helper_m), "the nested executables are untouched");
        assert_eq!(fs::read_to_string(s.dest().join(STAMP)).unwrap(), st);
    }

    #[test]
    fn a_matching_stamp_leaves_dest_and_removes_a_leftover_old_of_ours() {
        let s = Setup::new("current");
        fs::remove_dir_all(&s.staged).unwrap();
        let [host, helper, bundler] = exes(&s.t.join("target/web-host/debug"));
        let st = stamp(&host, &helper, &bundler).unwrap();
        fake_app(&s.dest(), BUNDLE_ID, "existing");
        fs::write(s.dest().join(STAMP), &st).unwrap();
        let exe = s.dest().join("Contents/MacOS").join(HOST);
        fs::File::open(&exe).unwrap().set_modified(SystemTime::now() - Duration::from_secs(60)).unwrap();
        fake_app(&s.old(), BUNDLE_ID, "leftover");
        let before = snapshot(&s.dest());
        let done = install(&s.artifact, &st, s.uid, SystemTime::now(), || panic!("not rebundled")).unwrap();
        assert!(!done.placed && done.warnings.is_empty(), "{done:?}");
        assert_eq!(snapshot(&s.dest()), before, "dest untouched");
        assert!(!s.old().exists(), "leftover .old removed");
        assert_eq!(done.trigger, Some(exe), "the trigger is still emitted");
        // A foreign .old is kept, with a warning.
        fake_app(&s.old(), "com.example.other", "foreign");
        let done = install(&s.artifact, &st, s.uid, SystemTime::now(), || panic!("not rebundled")).unwrap();
        assert!(s.old().exists() && done.warnings.len() == 1, "{done:?}");
    }

    #[test]
    fn a_bundler_only_change_or_new_bundle_rules_rebundle() {
        let s = Setup::new("stale");
        fs::remove_dir_all(&s.staged).unwrap();
        let [host, helper, bundler] = exes(&s.t.join("target/web-host/debug"));
        let st = stamp(&host, &helper, &bundler).unwrap();
        let place_with = |stamp: &str| {
            install(&s.artifact, stamp, s.uid, SystemTime::now(), || bundle_from(&s, &host)).unwrap()
        };
        assert!(place_with(&st).placed);
        assert!(!place_with(&st).placed, "current");
        // The bundler relinked; the host and helper are unchanged.
        fs::File::open(&bundler).unwrap().set_modified(SystemTime::now()).unwrap();
        let relinked = stamp(&host, &helper, &bundler).unwrap();
        assert_ne!(relinked, st);
        assert!(place_with(&relinked).placed, "a bundler change rebundles");
        let rules = relinked.replace(&format!("rules {BUNDLE_RULES}\n"), &format!("rules {}\n", BUNDLE_RULES + 1));
        assert_ne!(rules, relinked);
        assert!(place_with(&rules).placed, "new bundle rules rebundle");
    }

    #[test]
    fn a_bundle_that_cant_be_placed_fails_install_and_keeps_dest() {
        let s = Setup::new("install-refused");
        fs::remove_dir_all(&s.staged).unwrap();
        let [host, helper, bundler] = exes(&s.t.join("target/web-host/debug"));
        let st = stamp(&host, &helper, &bundler).unwrap();
        fake_app(&s.dest(), "com.example.other", "foreign");
        let before = snapshot(&s.artifact);
        assert!(install(&s.artifact, &st, s.uid, SystemTime::now(), || bundle_from(&s, &host)).is_err());
        assert_eq!(snapshot(&s.artifact), before);
    }
}

/// The build script reads this build's `OUT_DIR` against `cargo metadata`;
/// a Cargo whose build-dir layout changed would make every build skip the
/// web host, so it fails here instead.
#[test]
fn this_builds_out_dir_has_the_shape_the_build_script_expects() {
    let out = std::process::Command::new(cargo())
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(workspace())
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let (target, build) = metadata_dirs(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let got = dirs(Path::new(env!("OUT_DIR")), &target, build.as_deref(), env!("GHOSTREALM_TARGET"));
    assert!(got.is_ok(), "{got:?}");
}
