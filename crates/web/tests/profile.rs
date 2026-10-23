//! Two hosts on one profile, against the real CEF host: the second says the
//! profile is in use and exits, the first keeps serving, and the first exits
//! once the app hangs up.
//!
//! Needs the bundled host (`mise run web`); skipped when it isn't built.

mod common;

use std::time::{Duration, Instant};

use common::{host_exe, running};
use ghostrealm_web::{Request, WebHost};

#[test]
fn a_second_host_on_one_profile_reports_it_in_use() {
    let Some(exe) = host_exe() else {
        eprintln!("skipping: web host not built (run `mise run web`)");
        return;
    };
    let cache = std::env::temp_dir().join(format!("ghostrealm-web-profile-{}", std::process::id()));
    std::env::set_var("GHOSTREALM_WEB_CACHE", &cache);
    let first = WebHost::spawn(&exe, None).expect("spawn first host");
    assert!(first.wait_ready(Duration::from_secs(20)), "first host never became ready");
    let pid = first.pid().expect("first host pid");

    let second = WebHost::spawn(&exe, None).expect("spawn second host");
    let until = Instant::now() + Duration::from_secs(20);
    while second.is_alive() {
        assert!(Instant::now() < until, "second host never exited");
        std::thread::sleep(Duration::from_millis(50));
    }
    let failure = second.failure().unwrap_or_default();
    assert!(failure.contains("in use"), "second host said {failure:?}");

    assert!(first.is_alive(), "the first host keeps running");
    assert!(first.send(&Request::Create {
        id: 1,
        url: "data:text/html,<p>still here".into(),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(first.wait_loaded(1, Duration::from_secs(20)));
    assert_eq!(first.eval(1, "1 + 1", Duration::from_secs(5)), Ok("2".into()));

    drop(first);
    let until = Instant::now() + Duration::from_secs(10);
    while running(pid) {
        assert!(Instant::now() < until, "first host still running after hang-up");
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_dir_all(&cache);
}
