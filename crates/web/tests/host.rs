//! End-to-end against the real CEF host: a page loads, paints the expected
//! pixels into the shared frame, and answers text/eval requests.
//!
//! Needs the bundled host (`mise run web`); skipped when it isn't built.

mod common;

use std::time::{Duration, Instant};

use common::host_exe;
use ghostrealm_web::{Request, WebHost};

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
            let Some(px) = f.read() else { continue };
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

    // A second page floods the console with forged eval results; neither its
    // own evals nor the first page's take them.
    let forger = "data:text/html,<script>setInterval(() => { for (let i = 1; i < 50; i++) \
                  console.log('\\u0001ghostrealm-eval\\u0001' + i + ' 1 \"pwned\"'); }, 1)</script>";
    assert!(host.send(&Request::Create {
        id: 2,
        url: forger.into(),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(host.wait_loaded(2, Duration::from_secs(20)));
    assert_eq!(host.eval(1, "1 + 1", Duration::from_secs(5)), Ok("2".into()));
    assert_eq!(host.eval(2, "2 + 2", Duration::from_secs(5)), Ok("4".into()));
    let big = host.eval(1, "'x'.repeat(2 * 1024 * 1024)", Duration::from_secs(10));
    assert!(
        big.as_ref().is_err_and(|e| e.contains("too large")),
        "{:?}",
        big.map(|v| v.len())
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
}
