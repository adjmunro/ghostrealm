//! End-to-end against the real CEF host: a page loads, paints the expected
//! pixels into the shared frame, and answers text/eval requests.
//!
//! Needs the bundled host (`mise run web`); skipped when it isn't built.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use ghostrealm_web::client::{HOST_ENV, HOST_NAME};
use ghostrealm_web::{Request, WebHost};

fn host_exe() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os(HOST_ENV) {
        return Some(PathBuf::from(p));
    }
    // Test binaries live in target/<profile>/deps; the bundle sits in target/<profile>.
    let profile_dir = std::env::current_exe()
        .ok()?
        .parent()?
        .parent()?
        .to_path_buf();
    let mac = profile_dir.join(format!("{HOST_NAME}.app/Contents/MacOS/{HOST_NAME}"));
    let linux = profile_dir.join(HOST_NAME).join(HOST_NAME);
    [mac, linux].into_iter().find(|p| p.is_file())
}

#[test]
fn page_paints_and_answers_text_and_eval() {
    let Some(exe) = host_exe() else {
        eprintln!("skipping: web host not built (run `mise run web`)");
        return;
    };
    let cache = std::env::temp_dir().join(format!("ghostrealm-web-test-{}", std::process::id()));
    std::env::set_var("GHOSTREALM_WEB_CACHE", &cache);
    let mut host = WebHost::spawn(&exe, None).expect("spawn host");
    assert!(
        host.wait_ready(Duration::from_secs(20)),
        "host never became ready"
    );

    let url = "data:text/html,<body style='margin:0;background:rgb(255,0,0)'><p>hello host</p>";
    assert!(host.send(&Request::Create {
        id: 1,
        url: url.into(),
        width: 64,
        height: 48,
        scale: 2.0
    }));
    assert!(
        host.wait_loaded(1, Duration::from_secs(20)),
        "page never loaded"
    );

    // The frame is physical-sized (64x48 at scale 2 = a 32x24 DIP view) and,
    // once the page paints (earlier frames are blank), red away from the text.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = None;
    while Instant::now() < deadline && seen.is_none_or(|(_, _, bgr)| bgr != [0, 0, 255]) {
        if let Some(f) = host.frame(1) {
            let (w, h) = (f.width as usize, f.height as usize);
            let px = f.pixels().expect("mapped frame");
            let i = ((h - 2) * w + (w - 2)) * 4;
            seen = Some((f.width, f.height, [px[i], px[i + 1], px[i + 2]]));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let (w, h, bgr) = seen.expect("no frame arrived");
    assert_eq!((w, h), (64, 48));
    assert_eq!(bgr, [0, 0, 255], "corner pixel should be red (BGRA)");

    let text = host.text(1, Duration::from_secs(5)).expect("page text");
    assert!(text.contains("hello host"), "text was {text:?}");
    assert_eq!(
        host.eval(1, "6 * 7", Duration::from_secs(5)),
        Ok("42".into())
    );
    assert!(host.eval(1, "nope(", Duration::from_secs(5)).is_err());

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
}
