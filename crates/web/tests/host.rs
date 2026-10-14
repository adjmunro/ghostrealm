//! End-to-end against the real CEF host: a page loads, paints the expected
//! pixels into the shared frame, answers text/eval requests, and hears each
//! mouse button.
//!
//! Needs the bundled host (`mise run web`); skipped when it isn't built.

mod common;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::host_exe;
use ghostrealm_web::protocol::mods;
use ghostrealm_web::{MouseAction, MouseButton, Request, WebHost};

/// A ready host on its own profile `ghostrealm-web-<tag>-<pid>` (removed by the
/// caller), or `None` when the host isn't built.
fn spawn(tag: &str) -> Option<(WebHost, PathBuf)> {
    // The host reads its profile from the environment at spawn; tests run in
    // parallel, so each sets and spawns in turn.
    static SPAWN: Mutex<()> = Mutex::new(());
    let Some(exe) = host_exe() else {
        eprintln!("skipping: web host not built (run `mise run web`)");
        return None;
    };
    let cache = std::env::temp_dir().join(format!("ghostrealm-web-{tag}-{}", std::process::id()));
    let host = {
        let _turn = SPAWN.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("GHOSTREALM_WEB_CACHE", &cache);
        WebHost::spawn(&exe, None).expect("spawn host")
    };
    assert!(
        host.wait_ready(Duration::from_secs(20)),
        "host never became ready"
    );
    Some((host, cache))
}

#[test]
fn page_paints_and_answers_text_and_eval() {
    let Some((mut host, cache)) = spawn("test") else {
        return;
    };

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

#[test]
fn right_and_middle_presses_reach_the_page() {
    let Some((mut host, cache)) = spawn("mouse") else {
        return;
    };
    // Logs each press event as `type:button:buttons` (moves only while a
    // button is held).
    let url = "data:text/html,<body style='margin:0;height:100vh;background:rgb(255,0,0)'><script>window.log = [];\
               for (const t of ['mousedown', 'mouseup', 'mousemove', 'click', 'auxclick', 'contextmenu'])\
               addEventListener(t, e => { if (t != 'mousemove' || e.buttons) \
               log.push(t + ':' + e.button + ':' + e.buttons); }, true);</script>";
    assert!(host.send(&Request::Create {
        id: 1,
        url: url.into(),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(
        host.wait_loaded(1, Duration::from_secs(20)),
        "page never loaded"
    );
    // Input reaches a page once it paints its content (earlier frames are
    // blank).
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.frame(1).and_then(|f| f.read()).is_some_and(|px| px[..3] == [0, 0, 255]) {
        assert!(Instant::now() < deadline, "the page never painted");
        std::thread::sleep(Duration::from_millis(50));
    }
    let mouse = |x, mods, action| {
        assert!(host.send(&Request::Mouse { id: 1, x, y: 10, mods, action }));
    };
    let (left, right, middle) = (MouseButton::Left, MouseButton::Right, MouseButton::Middle);
    mouse(10, 0, MouseAction::Move);
    mouse(10, mods::LEFT_MOUSE, MouseAction::Down { button: left, count: 1 });
    mouse(10, 0, MouseAction::Up { button: left, count: 1 });
    mouse(10, mods::RIGHT_MOUSE, MouseAction::Down { button: right, count: 1 });
    mouse(20, mods::RIGHT_MOUSE, MouseAction::Move);
    mouse(20, 0, MouseAction::Up { button: right, count: 1 });
    mouse(20, mods::MIDDLE_MOUSE, MouseAction::Down { button: middle, count: 1 });
    mouse(20, 0, MouseAction::Up { button: middle, count: 1 });

    let deadline = Instant::now() + Duration::from_secs(10);
    let log = loop {
        let log = host.eval(1, "log.join(' ')", Duration::from_secs(5)).expect("read the log");
        if log.matches("auxclick").count() == 2 || Instant::now() > deadline {
            break log;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let mut got: Vec<&str> = log.trim_matches('"').split(' ').collect();
    // Where `contextmenu` falls (press or release) is the platform's choice.
    let menu = got
        .iter()
        .position(|e| *e == "contextmenu:2:2" || *e == "contextmenu:2:0")
        .unwrap_or_else(|| panic!("no contextmenu: {log}"));
    got.remove(menu);
    assert_eq!(
        got,
        [
            "mousedown:0:1",
            "mouseup:0:0",
            "click:0:0",
            "mousedown:2:2",
            "mousemove:0:2",
            "mouseup:2:0",
            "auxclick:2:0",
            "mousedown:1:4",
            "mouseup:1:0",
            "auxclick:1:0",
        ],
        "{log}"
    );

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
}
