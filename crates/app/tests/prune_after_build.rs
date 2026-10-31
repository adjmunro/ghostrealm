//! The app build script's prune rules (`build/prune.rs`).

#[allow(dead_code)]
#[path = "../build/prune.rs"]
mod prune;

use std::fs;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ghostrealm-prune-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&d);
    fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn prunes_at_most_once_per_interval() {
    let d = tmpdir("throttle");
    let stamp = d.join(prune::STAMP);
    let now = SystemTime::now();
    assert!(prune::due(None, &stamp, now), "no stamp yet");
    assert!(stamp.exists(), "restamped");
    assert!(!prune::due(None, &stamp, now + Duration::from_secs(60)), "a minute later");
    assert!(prune::due(None, &stamp, now + prune::EVERY), "an interval later");
    assert!(!prune::due(None, &d.join("no/such/dir/stamp"), now), "a stamp it can't write");
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn the_opt_out_skips_it_and_leaves_the_stamp() {
    let d = tmpdir("opt-out");
    let stamp = d.join(prune::STAMP);
    let now = SystemTime::now();
    for v in ["1", "yes"] {
        assert!(!prune::due(Some(v), &stamp, now), "{v}");
    }
    assert!(!stamp.exists());
    for v in ["", "0"] {
        let _ = fs::remove_file(&stamp);
        assert!(prune::due(Some(v), &stamp, now), "{v:?} doesn't opt out");
    }
    fs::remove_dir_all(&d).unwrap();
}

#[test]
fn only_the_checkouts_own_target_without_cargo_target_dir() {
    let root = tmpdir("target");
    let out = root.join("target/debug/build/ghostrealm-0123/out");
    fs::create_dir_all(&out).unwrap();
    let other = root.join("elsewhere/debug/build/ghostrealm-0123/out");
    fs::create_dir_all(&other).unwrap();
    let target = fs::canonicalize(root.join("target")).unwrap();
    assert_eq!(prune::own_target(&root, &out, None), Some(target));
    assert_eq!(prune::own_target(&root, &out, Some("/x")), None);
    assert_eq!(prune::own_target(&root, &other, None), None);
    fs::remove_dir_all(&root).unwrap();
}
