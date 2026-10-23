//! End-to-end against the real CEF host: a page loads, paints the expected
//! pixels into the shared frame, answers text/eval requests (which a hostile
//! page can't tamper with), and hears each mouse button.
//!
//! Needs the bundled host (`mise run web`); skipped when it isn't built.

mod common;

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use common::{host_exe, shut_down};
use ghostrealm_web::protocol::{link_script, mods, Key, KeyKind};
use ghostrealm_web::{MouseAction, MouseButton, PopupKind, Request, WebHost};

/// A ready host on its own profile `ghostrealm-web-<tag>-<pid>` (removed by the
/// caller), or `None` when the host isn't built.
fn spawn(tag: &str) -> Option<(WebHost, PathBuf)> {
    spawn_with(tag, &[])
}

/// [`spawn`], with extra environment variables for the host.
fn spawn_with(tag: &str, env: &[(&str, &str)]) -> Option<(WebHost, PathBuf)> {
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
        for (k, v) in env {
            std::env::set_var(k, v);
        }
        let host = WebHost::spawn(&exe, None).expect("spawn host");
        for (k, _) in env {
            std::env::remove_var(k);
        }
        host
    };
    assert!(host.wait_ready(Duration::from_secs(20)), "host never became ready: {:?}", host.failure());
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

    let big = host.eval(1, "'x'.repeat(2 * 1024 * 1024)", Duration::from_secs(10));
    assert!(
        big.as_ref().is_err_and(|e| e.contains("too large")),
        "{:?}",
        big.map(|v| v.len())
    );
    // The cap applies in the page, before a huge value is sent anywhere.
    let start = Instant::now();
    let huge = host.eval(1, "'x'.repeat(200 * 1024 * 1024)", Duration::from_secs(10));
    assert!(huge.as_ref().is_err_and(|e| e.contains("too large")), "{:?}", huge.map(|v| v.len()));
    assert!(start.elapsed() < Duration::from_secs(4), "took {:?}", start.elapsed());

    // An eval still pending when its page closes fails at once.
    std::thread::scope(|scope| {
        let pending = scope.spawn(|| {
            let start = Instant::now();
            (host.eval(1, "new Promise(() => {})", Duration::from_secs(10)), start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(500));
        assert!(host.send(&Request::Close { id: 1 }));
        let (result, took) = pending.join().unwrap();
        assert!(result.as_ref().is_err_and(|e| e.contains("closed")), "{result:?}");
        assert!(took < Duration::from_secs(5), "took {took:?}");
    });

    shut_down(host, &cache);
}

#[test]
fn right_and_middle_presses_reach_the_page() {
    let Some((mut host, cache)) = spawn("mouse") else {
        return;
    };
    // Logs each press event as `type:button:buttons` (moves only while a
    // button is held) into the DOM, where evals (in their own world) read it.
    let url = "data:text/html,<body style='margin:0;height:100vh;background:rgb(255,0,0)'><script>window.log = [];\
               for (const t of ['mousedown', 'mouseup', 'mousemove', 'click', 'auxclick', 'contextmenu'])\
               addEventListener(t, e => { if (t != 'mousemove' || e.buttons) { \
               log.push(t + ':' + e.button + ':' + e.buttons); \
               document.body.dataset.log = log.join(' '); } }, true);</script>";
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
        let log = host.eval(1, "document.body.dataset.log || ''", Duration::from_secs(5)).expect("read the log");
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

    shut_down(host, &cache);
}

/// `html` as a `data:` URL.
fn data_url(html: &str) -> String {
    format!("data:text/html,{}", html.replace('%', "%25").replace('#', "%23"))
}

/// A page that replaces the globals and prototypes an eval would use, forbids
/// `eval` by CSP, and records in the DOM whether it can see an eval's globals.
const HOSTILE: &str = r#"<meta http-equiv="Content-Security-Policy" content="script-src 'unsafe-inline'">
<title>Hostile</title><p id="p">dom text</p><script>
window.secret = 'page global';
setInterval(() => { document.body.dataset.seen = typeof window.agentVar; }, 5);
const lie = () => '"lie"';
JSON.stringify = lie;
JSON.parse = lie;
console.log = lie;
window.eval = lie;
window.String = lie;
Object.prototype.toJSON = function () { return 'lie'; };
Array.prototype.map = function () { return ['lie']; };
Array.prototype.join = lie;
Promise.prototype.then = function () { return Promise.resolve('lie'); };
Object.defineProperty(Error.prototype, 'stack', { get: lie });
Error.prototype.toString = lie;
Function.prototype.toString = lie;
document.body.dataset.tampered = 'yes';
</script>"#;

#[test]
fn a_hostile_page_cannot_shape_eval_results() {
    let Some((host, cache)) = spawn("hostile") else {
        return;
    };
    assert!(host.send(&Request::Create {
        id: 1,
        url: data_url(HOSTILE),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    let eval = |js: &str| host.eval(1, js, Duration::from_secs(5));

    // The page's own world really is tampered with.
    assert_eq!(eval("document.body.dataset.tampered"), Ok("\"yes\"".into()));
    assert_eq!(eval("document.body.dataset.seen"), Ok("\"undefined\"".into()));

    // The DOM is shared.
    assert_eq!(eval("document.title"), Ok("\"Hostile\"".into()));
    assert_eq!(eval("document.querySelector('#p').textContent"), Ok("\"dom text\"".into()));
    // Globals, prototypes and the encoding are not.
    assert_eq!(eval("({ a: [1, 2].map(x => x * 2), s: String(3) })"), Ok(r#"{"a":[2,4],"s":"3"}"#.into()));
    assert_eq!(eval("[1, 2].join('-')"), Ok("\"1-2\"".into()));
    assert_eq!(eval("Promise.resolve(5).then(x => x + 1)"), Ok("6".into()));
    assert_eq!(eval("JSON.parse('[1]')"), Ok("[1]".into()));
    assert_eq!(eval("typeof secret"), Ok("\"undefined\"".into()));
    assert_eq!(eval("let a = 1; a + 1"), Ok("2".into()), "statement lists still run");
    let err = eval("throw new Error('boom')").unwrap_err();
    assert!(err.contains("Error: boom"), "{err}");
    assert!(eval("nope(").unwrap_err().contains("SyntaxError"));

    // An eval's globals persist for the document, unseen by the page.
    assert_eq!(eval("window.agentVar = 7"), Ok("7".into()));
    assert_eq!(eval("agentVar"), Ok("7".into()));
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(eval("document.body.dataset.seen"), Ok("\"undefined\"".into()));

    // A new document gets a new world.
    host.expect_load(1);
    assert!(host.send(&Request::Navigate {
        id: 1,
        url: data_url(&HOSTILE.replace("Hostile", "Second")),
    }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "second page never loaded");
    assert_eq!(eval("document.title"), Ok("\"Second\"".into()));
    assert_eq!(eval("typeof agentVar"), Ok("\"undefined\"".into()));
    assert_eq!(eval("[3].map(x => x + 1)"), Ok("[4]".into()));

    shut_down(host, &cache);
}

#[test]
fn a_dead_renderer_is_reported_and_reload_revives_it() {
    let Some((host, cache)) = spawn("crash") else {
        return;
    };
    let page = "data:text/html,<title>Alive</title>";
    assert!(host.send(&Request::Create {
        id: 1,
        url: page.into(),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    assert_eq!(host.eval(1, "1 + 1", Duration::from_secs(5)), Ok("2".into()));

    assert!(host.send(&Request::Navigate {
        id: 1,
        url: "chrome://crash".into()
    }));
    let deadline = Instant::now() + Duration::from_secs(20);
    let gone = loop {
        if let Some(gone) = host.view(1).and_then(|v| v.gone) {
            break gone;
        }
        assert!(Instant::now() < deadline, "the renderer's death was never reported");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert!(gone.starts_with("the page's renderer "), "{gone}");

    // Evals fail at once until it is reloaded.
    let start = Instant::now();
    let dead = host.eval(1, "1 + 1", Duration::from_secs(10));
    assert!(dead.as_ref().is_err_and(|e| e.contains("renderer")), "{dead:?}");
    assert!(start.elapsed() < Duration::from_secs(2), "took {:?}", start.elapsed());

    host.expect_load(1);
    assert!(host.send(&Request::Reload { id: 1 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "reload never loaded");
    assert_eq!(host.view(1).and_then(|v| v.gone), None, "the reload clears it");
    assert_eq!(host.eval(1, "document.title", Duration::from_secs(5)), Ok("\"Alive\"".into()));

    shut_down(host, &cache);
}

/// A local HTTP server: `/a` and `/b` are pages (each links to the other);
/// `/opener` opens `/popup` on a click (red while it waits for one) and logs
/// messages it gets in `data-got`; `/popup` posts to its opener and closes
/// itself; `/stay` is a page; `/opentab` opens `/stay` in a new tab on a
/// click, then sets `data-opened`; `/link` links to `/stay` with no target,
/// `/aboutlink` to `about:blank`. `/opener` and the link pages count their
/// clicks (the link pages' middle clicks too) in `data-clicks`. `/hit...`
/// paths are recorded in the returned list. Anything else is empty.
fn serve() -> (String, std::sync::Arc<Mutex<Vec<String>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = hits.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut line = String::new();
            if BufReader::new(&stream).read_line(&mut line).is_err() {
                continue;
            }
            let path = line.split(' ').nth(1).unwrap_or("/").to_string();
            let body = match path.as_str() {
                "/a" => "<title>A</title><a id=\"l\" href=\"/b\">to b</a>".to_string(),
                "/b" => "<title>B</title><a id=\"l\" href=\"/a\">to a</a>".to_string(),
                "/opener" => OPENER.to_string(),
                "/popup" => "<title>Popup</title><script>window.opener.postMessage('from ' + \
                             location.pathname, '*'); window.close();</script>"
                    .to_string(),
                "/stay" => "<title>Stay</title><p>stay text</p>".to_string(),
                "/blank" => LINK.replace("LINK", "target=_blank href=/stay"),
                "/selfclose" => LINK.replace("LINK", "target=_blank href=/stay onclick='setTimeout(() => window.close(), 0)'"),
                "/opentab" => LINK.replace("LINK", "onclick=\"window.open('/stay'); document.body.dataset.opened = 1\""),
                "/link" => format!("<title>Link</title>{}", LINK.replace("LINK", "href=/stay")),
                "/aboutlink" => LINK.replace("LINK", "href=about:blank"),
                _ => {
                    if path.starts_with("/hit") {
                        seen.lock().unwrap().push(path);
                    }
                    String::new()
                }
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (base, hits)
}

const OPENER: &str = "<title>Opener</title><body style='margin:0'>\
<button style='position:fixed;inset:0;border:0;background:rgb(255,0,0)' \
onclick=\"window.open('/popup', 'p', 'width=300,height=200')\">open</button><script>\
addEventListener('message', e => { document.body.dataset.got = \
(document.body.dataset.got || '') + e.data + (e.source ? '' : ' (no source)') + ';'; });</script>\
<script>addEventListener('click', () => document.body.dataset.clicks = \
+(document.body.dataset.clicks || 0) + 1, true)</script>";

/// A red link over the whole page.
const LINK: &str = "<body style='margin:0'><a LINK style='position:fixed;inset:0;background:rgb(255,0,0)'>x</a>\
<script>for (const t of ['click', 'auxclick']) addEventListener(t, () => document.body.dataset.clicks = \
+(document.body.dataset.clicks || 0) + 1, true)</script>";

/// The modifier that makes a clicked link a new background tab: Cmd on
/// macOS, Ctrl elsewhere.
const NEW_TAB: u32 = if cfg!(target_os = "macos") { mods::COMMAND } else { mods::CONTROL };

/// [`NEW_TAB`] as a `MouseEvent` init property.
const NEW_TAB_KEY: &str = if cfg!(target_os = "macos") { "metaKey" } else { "ctrlKey" };

/// Wait for page `id`'s first painted frame whose top-left pixel is `bgr`.
fn wait_painted(host: &mut WebHost, id: u64, bgr: [u8; 3]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.frame(id).and_then(|f| f.read()).is_some_and(|px| px[..3] == bgr) {
        assert!(Instant::now() < deadline, "page {id} never painted");
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Click page `id` near its top-left until it opens a pop-up, and return
/// it. A page that has just painted may not hit-test input yet, so a click
/// can go unheard. It clicks again only while the page counts no click
/// (`data-clicks`): a click it heard opens its pop-up, however slowly, and
/// a second would open or reload another.
fn click_for_popup(host: &WebHost, id: u64) -> ghostrealm_web::PopupInfo {
    click_for_popup_with(host, id, 0, MouseButton::Left)
}

/// [`click_for_popup`] with `button`, the modifiers `flags` held.
fn click_for_popup_with(host: &WebHost, id: u64, flags: u32, button: MouseButton) -> ghostrealm_web::PopupInfo {
    let held = match button {
        MouseButton::Left => mods::LEFT_MOUSE,
        MouseButton::Middle => mods::MIDDLE_MOUSE,
        MouseButton::Right => mods::RIGHT_MOUSE,
    };
    let click = |mods, action| assert!(host.send(&Request::Mouse { id, x: 20, y: 20, mods, action }));
    // A page that closed itself after the click answers with an error: heard.
    let unheard = || host.eval(id, "+(document.body.dataset.clicks || 0)", Duration::from_secs(5)) == Ok("0".into());
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut clicked: Option<Instant> = None;
    loop {
        let mut got = host.take_popups(id);
        if !got.is_empty() {
            // Only a click the page hadn't counted when asked can open more.
            for extra in got.drain(1..) {
                host.send(&Request::Close { id: extra.id });
            }
            return got.remove(0);
        }
        assert!(Instant::now() < deadline, "no click on page {id} opened a pop-up");
        if clicked.is_none_or(|t| t.elapsed() >= Duration::from_secs(2)) && unheard() {
            click(flags, MouseAction::Move);
            click(flags | held, MouseAction::Down { button, count: 1 });
            click(flags, MouseAction::Up { button, count: 1 });
            clicked = Some(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Click page `id` near its top-left until `js` (evaluated in it) is truthy:
/// a page that has just painted may not hit-test input yet, so a click can
/// go unheard. It clicks again only while `js` is falsy, so a click the
/// page heard is never repeated.
fn click_until(host: &WebHost, id: u64, js: &str) {
    click_at_until(host, id, (20, 20), 0, js);
}

/// [`click_until`] at `(x, y)`, the modifiers `flags` held.
fn click_at_until(host: &WebHost, id: u64, (x, y): (i32, i32), flags: u32, js: &str) {
    let heard = || host.eval(id, &format!("!!({js})"), Duration::from_secs(5)) == Ok("true".into());
    for _ in 0..5 {
        for (mods, action) in [
            (flags, MouseAction::Move),
            (flags | mods::LEFT_MOUSE, MouseAction::Down { button: MouseButton::Left, count: 1 }),
            (flags, MouseAction::Up { button: MouseButton::Left, count: 1 }),
        ] {
            assert!(host.send(&Request::Mouse { id, x, y, mods, action }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if heard() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    panic!("page {id} never heard a click");
}

/// The pop-ups page `opener` opened, once there are `n`.
fn wait_popups(host: &WebHost, opener: u64, n: usize) -> Vec<ghostrealm_web::PopupInfo> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut got = Vec::new();
    while got.len() < n {
        got.extend(host.take_popups(opener));
        assert!(Instant::now() < deadline, "pop-ups so far: {got:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    got
}

fn wait_closed(host: &WebHost, id: u64) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.is_closed(id) {
        assert!(Instant::now() < deadline, "page {id} never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// `data-got` of page `id` once it holds `want`.
fn wait_got(host: &WebHost, id: u64, want: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got = host.eval(id, "document.body.dataset.got || ''", Duration::from_secs(5)).unwrap();
        if got.contains(want) || Instant::now() > deadline {
            return got;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn a_clicked_popup_keeps_its_opener_and_closes_itself() {
    let Some((mut host, cache)) = spawn("popup") else {
        return;
    };
    let (base, _) = serve();
    assert!(host.send(&Request::Create { id: 1, url: format!("{base}/opener"), width: 800, height: 600, scale: 2.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);

    let p = click_for_popup(&host, 1);
    assert!(p.id >= ghostrealm_web::protocol::POPUP_IDS, "{p:?}");
    assert!(p.from_input, "a click opened it");
    assert_eq!(p.kind, PopupKind::Popup, "window.open with a size");
    assert!(p.url.ends_with("/popup"), "{p:?}");
    assert_eq!((p.width, p.height), (600, 400), "the size it asked for, in physical pixels");

    // It posts to its opener, then closes itself.
    wait_closed(&host, p.id);
    // A click still on its way when the page was asked for its count can
    // make a second pop-up (or load the first again): each posts the same
    // message, from a source. Each post needs a click the page heard, so a
    // message delivered twice still fails.
    let got = wait_got(&host, 1, "from /popup");
    let posts: Vec<&str> = got.trim_matches('"').split_terminator(';').collect();
    assert!(!posts.is_empty() && posts.iter().all(|m| *m == "from /popup"), "{got}");
    let clicks = host.eval(1, "+(document.body.dataset.clicks || 0)", Duration::from_secs(5)).unwrap();
    assert!(posts.len() <= clicks.parse::<usize>().unwrap(), "{got} from {clicks} clicks");
    assert!(!host.is_closed(1), "the opener stays");

    shut_down(host, &cache);
}

#[test]
fn a_popup_says_how_the_page_asked_for_it() {
    let Some((mut host, cache)) = spawn("popup-kind") else {
        return;
    };
    let (base, _) = serve();
    let mut id = 0;
    let mut click_on = |host: &mut WebHost, path: &str| {
        id += 1;
        assert!(host.send(&Request::Create { id, url: format!("{base}{path}"), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
        click_for_popup(host, id)
    };
    let tab = click_on(&mut host, "/blank");
    assert_eq!(tab.kind, PopupKind::Tab, "target=_blank");
    assert_eq!(host.eval_as(1, "window.open('/stay') !== null", true, Duration::from_secs(5)), Ok("true".into()));
    let p = wait_popups(&host, 1, 1);
    assert_eq!(p[0].kind, PopupKind::Tab, "window.open with no features");
    let huge = "window.open('/stay', '', 'width=40000,height=40000') !== null";
    assert_eq!(host.eval_as(1, huge, true, Duration::from_secs(5)), Ok("true".into()));
    let q = wait_popups(&host, 1, 1).remove(0);
    assert_eq!((q.kind, q.width, q.height), (PopupKind::Popup, 100, 100), "no larger than its opener");

    // A tab a person opened outlives its opener; a pop-up window closes with it.
    assert!(host.send(&Request::Close { id: 1 }));
    wait_closed(&host, q.id);
    std::thread::sleep(Duration::from_millis(300));
    assert!(!host.is_closed(tab.id));
    assert!(host.wait_loaded(tab.id, Duration::from_secs(20)));
    assert_eq!(host.eval(tab.id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()));

    shut_down(host, &cache);
}

/// Given a page that opens a person's tab (A) on a click and then closes
/// itself, and a page that opens a person's tab (B) on a click;
/// when A's opener closes itself, and B's opener is closed and forgotten
/// by the app after B reached the client but before its view took B;
/// then A waits for the opener's view to take it and still runs, and B is
/// handed to the app as a tab of the forgotten opener and still runs.
#[test]
fn a_persons_tab_outlives_its_opener() {
    let Some((mut host, cache)) = spawn("popup-orphan") else {
        return;
    };
    let (base, _) = serve();
    let open = |host: &mut WebHost, id, path: &str| {
        assert!(host.send(&Request::Create { id, url: format!("{base}{path}"), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
    };

    // The opener closes itself as its link opens a tab: the app still holds
    // the opener, so the tab waits for its view to take it.
    open(&mut host, 1, "/selfclose");
    let tab = click_for_popup(&host, 1);
    wait_closed(&host, 1);
    assert!(host.wait_loaded(tab.id, Duration::from_secs(20)), "the tab lives on");
    assert_eq!(host.eval(tab.id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()));
    host.forget(tab.id);

    // An opener the app closes and forgets before its view takes the tab.
    // `window.open` returns once the host has reported the tab, so the
    // client has it before the eval that sees `data-opened` answers. (A tab
    // reported after the forget is the client tests'.)
    open(&mut host, 2, "/opentab");
    click_until(&host, 2, "document.body.dataset.opened");
    assert!(host.send(&Request::Close { id: 2 }));
    host.forget(2);
    let deadline = Instant::now() + Duration::from_secs(10);
    let tabs = loop {
        let tabs = host.take_orphan_tabs();
        if !tabs.is_empty() {
            break tabs;
        }
        assert!(Instant::now() < deadline, "the tab was never handed over");
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(tabs.iter().all(|t| t.opener == 2 && t.is_tab()), "{tabs:?}");
    assert!(host.wait_loaded(tabs[0].id, Duration::from_secs(20)), "the tab lives on");
    assert_eq!(host.eval(tabs[0].id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()));
    for t in tabs {
        host.send(&Request::Close { id: t.id });
        host.forget(t.id);
    }

    shut_down(host, &cache);
}

/// Given a page covered by a link to `/stay` (no target);
/// when a person clicks it with Cmd (Ctrl off macOS), Shift, Cmd+Shift, the
/// middle button, or Shift+middle;
/// then each click opens `/stay` as a person's new tab (a background tab, a
/// window, a foreground tab, a background tab, a foreground tab), and the
/// page stays where it was.
/// Given a page covered by a `target=_blank` link;
/// when a person Cmd-clicks it;
/// then it opens as a person's background tab.
/// Given a page covered by a link to `about:blank`;
/// when a person Cmd-clicks it;
/// then no tab opens and the link loads in place, as a plain click's would.
#[test]
fn a_modified_or_middle_clicked_link_opens_a_tab() {
    let Some((mut host, cache)) = spawn("modified-click") else {
        return;
    };
    let (base, _) = serve();
    let (left, middle) = (MouseButton::Left, MouseButton::Middle);
    let cases = [
        (NEW_TAB, left, PopupKind::BackgroundTab),
        (mods::SHIFT, left, PopupKind::Window),
        (NEW_TAB | mods::SHIFT, left, PopupKind::Tab),
        (0, middle, PopupKind::BackgroundTab),
        (mods::SHIFT, middle, PopupKind::Tab),
    ];
    let open = |host: &mut WebHost, id, path: &str| {
        assert!(host.send(&Request::Create { id, url: format!("{base}{path}"), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
    };
    for (id, (flags, button, kind)) in (1..).zip(cases) {
        open(&mut host, id, "/link");
        let tab = click_for_popup_with(&host, id, flags, button);
        let case = format!("mods {flags:#x}, {button:?}: {tab:?}");
        assert!(tab.from_input && tab.is_tab(), "{case}");
        assert_eq!(tab.kind, kind, "{case}");
        assert_eq!(tab.url, format!("{base}/stay"), "{case}");
        assert!(host.wait_loaded(tab.id, Duration::from_secs(20)), "{case}");
        assert_eq!(host.eval(tab.id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()), "{case}");
        assert_eq!(host.eval(id, "document.title", Duration::from_secs(5)), Ok("\"Link\"".into()), "{case}");
        assert_eq!(host.view(id).unwrap().url, format!("{base}/link"), "{case}");
        assert!(host.take_popups(id).is_empty(), "one click, one tab: {case}");
    }

    // A `target=_blank` link clicked so is the same.
    let id = cases.len() as u64 + 1;
    open(&mut host, id, "/blank");
    let tab = click_for_popup_with(&host, id, NEW_TAB, left);
    assert!(tab.from_input && tab.kind == PopupKind::BackgroundTab, "{tab:?}");
    assert_eq!(tab.url, format!("{base}/stay"));

    // A link that isn't http(s) loads in place. A page that has just painted
    // may not hit-test input yet, so it clicks until the page leaves.
    let id = cases.len() as u64 + 2;
    open(&mut host, id, "/aboutlink");
    let deadline = Instant::now() + Duration::from_secs(20);
    while host.view(id).unwrap().url != "about:blank" {
        assert!(Instant::now() < deadline, "the link never loaded in place");
        for (flags, action) in [
            (NEW_TAB, MouseAction::Move),
            (NEW_TAB | mods::LEFT_MOUSE, MouseAction::Down { button: left, count: 1 }),
            (NEW_TAB, MouseAction::Up { button: left, count: 1 }),
        ] {
            assert!(host.send(&Request::Mouse { id, x: 20, y: 20, mods: flags, action }));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(host.take_popups(id).is_empty());

    shut_down(host, &cache);
}

/// Given a page covered by a link to `/stay` that no person has touched;
/// when an eval without a gesture dispatches a Cmd-click (Ctrl off macOS) on
/// it, then gesture evals dispatch one more than the pop-up cap;
/// then the gestureless click opens nothing, each gesture click up to the
/// cap opens `/stay` as an agent's background pop-up (hidden, drivable), the
/// one past the cap is refused, the page never leaves, and closing it closes
/// them.
#[test]
fn an_agents_modified_click_opens_a_hidden_popup() {
    let Some((host, cache)) = spawn("modified-click-agent") else {
        return;
    };
    let (base, _) = serve();
    assert!(host.send(&Request::Create { id: 1, url: format!("{base}/link"), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    let click = format!(
        "document.querySelector('a').dispatchEvent(new MouseEvent('click', \
         {{ bubbles: true, cancelable: true, {NEW_TAB_KEY}: true }})); 1"
    );
    let click = |gesture| host.eval_as(1, &click, gesture, Duration::from_secs(5)) == Ok("1".into());

    // The host hears the clicks in order: the gestureless one came first.
    assert!(click(false));
    assert!(click(true));
    let [p] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert!(!p.from_input && !p.is_tab(), "{p:?}");
    assert_eq!(p.kind, PopupKind::BackgroundTab, "{p:?}");
    assert_eq!(p.url, format!("{base}/stay"));
    assert!(host.wait_loaded(p.id, Duration::from_secs(20)), "pop-up never loaded");
    assert_eq!(host.eval(p.id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()));

    let max = ghostrealm_web::protocol::MAX_POPUPS;
    for _ in 1..=max {
        assert!(click(true));
    }
    let mut open = wait_popups(&host, 1, max - 1);
    open.push(p);
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.view(1).unwrap().popups_refused == 0 {
        assert!(Instant::now() < deadline, "the click past the cap was never refused");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(host.view(1).unwrap().popups_refused, 1);
    assert!(host.take_popups(1).is_empty());
    assert_eq!(host.eval(1, "document.title", Duration::from_secs(5)), Ok("\"Link\"".into()));
    assert_eq!(host.view(1).unwrap().url, format!("{base}/link"));

    assert!(host.send(&Request::Close { id: 1 }));
    wait_closed(&host, 1);
    for q in open {
        wait_closed(&host, q.id);
    }

    shut_down(host, &cache);
}

/// One request [`serve_sites`] recorded.
#[derive(Debug, Clone)]
struct Hit {
    method: String,
    path: String,
    cookie: String,
    site: String,
    referer: String,
}

/// A local HTTP server as two sites: `http://127.0.0.1:<port>` and
/// `http://localhost:<port>`, returned with the `/hit...` requests it got.
/// `/cookies` sets a `SameSite=Strict` cookie `s` and a `Lax` one `l`;
/// `/link?<url>` is a link to `<url>` over the page (`/nrlink?<url>` one with
/// `rel=NoReferrer`, `/nplink?<url>` one with `referrerpolicy=no-referrer`,
/// `/closedlink?<url>` one in a closed shadow root, `/sandboxedlink?<url>`
/// one in a document sandboxed by CSP (`allow-scripts allow-popups`;
/// `/sealedlink?<url>` without `allow-popups`),
/// `/beacon?<url>` one that requests `/hitclick` on each click);
/// `/hostile?<url>` is a button over the page that answers a click by
/// sending three Cmd-clicks (Ctrl off macOS) to a hidden link to `<url>`;
/// `/sandboxed?<url>` is a
/// `sandbox=allow-scripts` frame over the page holding `/link?<url>`, which
/// tells its parent of each click; `/post` is a form posting to
/// `/hitpost`, its submit button over the top half, its text field `q`
/// below. Pages count their clicks in `data-clicks`.
fn serve_sites() -> (String, String, std::sync::Arc<Mutex<Vec<Hit>>>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = std::sync::Arc::new(Mutex::new(Vec::new()));
    let seen = hits.clone();
    const COUNT: &str = "<script>for (const t of ['click', 'auxclick']) addEventListener(t, () => \
        document.body.dataset.clicks = +(document.body.dataset.clicks || 0) + 1, true); \
        addEventListener('message', () => document.body.dataset.clicks = +(document.body.dataset.clicks || 0) + 1)</script>";
    const FULL: &str = "position:fixed;inset:0;border:0;background:rgb(255,0,0)";
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let seen = seen.clone();
            // A connection per thread: Chromium opens some it sends nothing on.
            std::thread::spawn(move || {
            let mut reader = BufReader::new(&stream);
            let mut lines = Vec::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                lines.push(line.trim_end().to_string());
            }
            let Some(first) = lines.first() else { return };
            let mut parts = first.split(' ');
            let method = parts.next().unwrap_or_default().to_string();
            let path = parts.next().unwrap_or("/").to_string();
            let header = |name: &str| {
                lines.iter().find_map(|l| l.split_once(": ").filter(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.to_string()))
            };
            let (route, arg) = path.split_once('?').unwrap_or((&path, ""));
            let mut extra = String::new();
            let body = match route {
                "/cookies" => {
                    extra = "Set-Cookie: s=1; SameSite=Strict\r\nSet-Cookie: l=1; SameSite=Lax\r\n".into();
                    "<title>Cookies</title>".to_string()
                }
                "/link" => format!("<title>Link</title><body style='margin:0'><a href='{arg}' style='{FULL}'>x</a>{COUNT}"),
                "/beacon" => format!(
                    "<body style='margin:0'><a href='{arg}' style='{FULL}' \
                     onclick=\"fetch('/hitclick', {{ keepalive: true }})\">x</a>"
                ),
                "/nrlink" => format!("<body style='margin:0'><a rel=NoReferrer href='{arg}' style='{FULL}'>x</a>{COUNT}"),
                "/closedlink" => format!(
                    "<body style='margin:0'><div id=h></div><script>h.attachShadow({{ mode: 'closed' }}).innerHTML = \
                     \"<a href='{arg}' style='{FULL}'>x</a>\"</script>{COUNT}"
                ),
                "/sandboxedlink" | "/sealedlink" => {
                    let popups = if route == "/sandboxedlink" { " allow-popups" } else { "" };
                    extra = format!("Content-Security-Policy: sandbox allow-scripts{popups}\r\n");
                    format!("<body style='margin:0'><a href='{arg}' style='{FULL}'>x</a>{COUNT}")
                }
                "/nplink" => format!("<body style='margin:0'><a referrerpolicy=no-referrer href='{arg}' style='{FULL}'>x</a>{COUNT}"),
                "/hostile" => format!(
                    "<title>Hostile</title><body style='margin:0'><a id=l href='{arg}' style='display:none'>x</a>\
                     <button style='{FULL}' onclick=\"for (let i = 0; i < 3; i++) l.dispatchEvent(new MouseEvent('click', \
                     {{ bubbles: true, cancelable: true, {NEW_TAB_KEY}: true }}))\">go</button>{COUNT}"
                ),
                "/sandboxed" => format!(
                    "<title>Sandboxed</title><body style='margin:0'><iframe sandbox=allow-scripts \
                     src='/framed?{arg}' style='{FULL}'></iframe>{COUNT}"
                ),
                "/framed" => format!(
                    "<body style='margin:0'><a href='{arg}' style='{FULL}'>x</a><script>addEventListener('click', \
                     () => parent.postMessage('click', '*'), true)</script>"
                ),
                "/post" => format!(
                    "<title>Post</title><body style='margin:0'><form method=post action=/hitpost>\
                     <button style='position:fixed;left:0;top:0;right:0;height:50%;border:0;background:rgb(255,0,0)'>go</button>\
                     <input name=q style='position:fixed;left:0;bottom:0;width:100%;height:40%'></form>{COUNT}"
                ),
                _ => {
                    if route.starts_with("/hit") {
                        seen.lock().unwrap().push(Hit {
                            method,
                            path: path.clone(),
                            cookie: header("Cookie").unwrap_or_default(),
                            site: header("Sec-Fetch-Site").unwrap_or_default(),
                            referer: header("Referer").unwrap_or_default(),
                        });
                    }
                    format!("<title>{route}</title>")
                }
            };
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n{extra}Content-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
            });
        }
    });
    (format!("http://127.0.0.1:{port}"), format!("http://localhost:{port}"), hits)
}

/// The requests for `path` among `hits`, once there are `n`.
fn wait_hits(hits: &Mutex<Vec<Hit>>, path: &str, n: usize) -> Vec<Hit> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let got: Vec<Hit> = hits.lock().unwrap().iter().filter(|h| h.path == path).cloned().collect();
        if got.len() >= n {
            return got;
        }
        assert!(Instant::now() < deadline, "requests for {path}: {got:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Given a page on one site linking to a page on another, which holds a
/// `SameSite=Strict` and a `Lax` cookie;
/// when a person Cmd-clicks (Ctrl off macOS) the link;
/// then its tab's request is the page's own: cross-site, with the `Lax`
/// cookie only and the page's origin as referrer, as for a plain click.
/// Given a page whose button answers a click by sending three Cmd-clicks to
/// a hidden link to that other site;
/// when a person plainly clicks the button;
/// then one tab opens, its request cross-site without the `Strict` cookie.
#[test]
fn a_modified_click_loads_as_its_page_asked() {
    let Some((mut host, cache)) = spawn("modified-click-site") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    assert!(host.send(&Request::Create { id: 1, url: format!("{a}/cookies"), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    let open = |host: &mut WebHost, id, url: String| {
        assert!(host.send(&Request::Create { id, url, width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
    };

    open(&mut host, 2, format!("{b}/link?{a}/hit-link"));
    let tab = click_for_popup_with(&host, 2, NEW_TAB, MouseButton::Left);
    assert!(tab.from_input && tab.kind == PopupKind::BackgroundTab, "{tab:?}");
    let [hit] = <[_; 1]>::try_from(wait_hits(&hits, "/hit-link", 1)).unwrap();
    assert_eq!((hit.method.as_str(), hit.site.as_str(), hit.cookie.as_str()), ("GET", "cross-site", "l=1"), "{hit:?}");
    assert_eq!(hit.referer, format!("{b}/"), "{hit:?}");

    open(&mut host, 3, format!("{b}/hostile?{a}/hit-hostile"));
    // Chromium makes a script's background tab a foreground one.
    let tab = click_for_popup(&host, 3);
    assert!(tab.from_input && tab.is_tab(), "{tab:?}");
    wait_hits(&hits, "/hit-hostile", 1);
    std::thread::sleep(Duration::from_millis(500));
    assert!(host.take_popups(3).is_empty(), "one press, one tab");
    let got = wait_hits(&hits, "/hit-hostile", 1);
    assert_eq!(got.len(), 1, "{got:?}");
    assert_eq!((got[0].site.as_str(), got[0].cookie.as_str()), ("cross-site", "l=1"), "{got:?}");

    shut_down(host, &cache);
}

/// Given pages on one site whose links to another site withhold the referrer
/// (`rel=NoReferrer`, `referrerpolicy=no-referrer`), and one whose link is
/// in a closed shadow root, out of the host's sight;
/// when a person Cmd-clicks (Ctrl off macOS) each;
/// then each tab's request has no referrer, as a plain click's wouldn't for
/// the first two.
#[test]
fn a_modified_click_keeps_a_links_referrer_policy() {
    let Some((mut host, cache)) = spawn("modified-click-referrer") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    for (id, route) in [(1, "nrlink"), (2, "nplink"), (3, "closedlink")] {
        assert!(host.send(&Request::Create { id, url: format!("{b}/{route}?{a}/hit-{route}"), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(&mut host, id, [0, 0, 255]);
        let tab = click_for_popup_with(&host, id, NEW_TAB, MouseButton::Left);
        assert!(tab.from_input && tab.is_tab(), "{tab:?}");
        let [hit] = <[_; 1]>::try_from(wait_hits(&hits, &format!("/hit-{route}"), 1)).unwrap();
        assert_eq!((hit.site.as_str(), hit.referer.as_str()), ("cross-site", ""), "{hit:?}");
    }

    shut_down(host, &cache);
}

/// Given a page on one site covered by a link to another;
/// when the host's script for that link runs in it as a gesture eval, first
/// as asked by a document of another origin, then as asked by the page;
/// then the first does nothing (`"moved"`), and the second opens the link
/// in a new tab, requested once.
#[test]
fn the_link_script_runs_only_in_the_document_that_asked() {
    let Some((host, cache)) = spawn("link-script") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    let page = format!("{b}/link?{a}/hit-guard");
    assert!(host.send(&Request::Create { id: 1, url: page.clone(), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    let url = format!("{a}/hit-guard");
    let run = |from: &str| host.eval_as(1, &link_script(from, &url, false), true, Duration::from_secs(5));
    assert_eq!(run("http://elsewhere.test/page"), Ok("\"moved\"".into()));
    assert_eq!(run(&page), Ok("\"opened\"".into()));
    // The host hears pop-ups in order: the first eval opened none.
    let [p] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert_eq!(p.url, url);
    wait_hits(&hits, "/hit-guard", 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(wait_hits(&hits, "/hit-guard", 1).len(), 1);

    shut_down(host, &cache);
}

/// Send page `id` Cmd-clicks (Ctrl off macOS) near its top-left, with no
/// eval between them, until the server has `n` `/hitclick` requests: a page
/// that has just painted may not hit-test input yet. A click the page heard
/// is never repeated.
fn raw_click_until_heard(host: &WebHost, id: u64, hits: &Mutex<Vec<Hit>>, n: usize) {
    let heard = || hits.lock().unwrap().iter().filter(|h| h.path == "/hitclick").count() >= n;
    for _ in 0..5 {
        for (flags, action) in [
            (NEW_TAB, MouseAction::Move),
            (NEW_TAB | mods::LEFT_MOUSE, MouseAction::Down { button: MouseButton::Left, count: 1 }),
            (NEW_TAB, MouseAction::Up { button: MouseButton::Left, count: 1 }),
        ] {
            assert!(host.send(&Request::Mouse { id, x: 20, y: 20, mods: flags, action }));
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if heard() {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    panic!("page {id} never heard a click");
}

/// Given a page whose link (to another site) has opened a tab from a
/// person's Cmd-click (Ctrl off macOS);
/// when the page goes to another such page, and the person Cmd-clicks its
/// link, with no eval between the clicks (as from the app);
/// then that click opens a tab too.
#[test]
fn a_modified_click_after_a_navigation_opens_a_tab() {
    let Some((mut host, cache)) = spawn("modified-click-navigate") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    assert!(host.send(&Request::Create { id: 1, url: format!("{b}/beacon?{a}/hit-first"), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);
    raw_click_until_heard(&host, 1, &hits, 1);
    let [p] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert_eq!(p.url, format!("{a}/hit-first"));

    host.expect_load(1);
    assert!(host.send(&Request::Navigate { id: 1, url: format!("{b}/beacon?{a}/hit-second") }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "second page never loaded");
    raw_click_until_heard(&host, 1, &hits, 2);
    let [q] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert_eq!(q.url, format!("{a}/hit-second"));

    shut_down(host, &cache);
}

/// Given a host that runs a modified click's eval 3 s late (at 1.5 s it
/// failed by itself here, its DevTools session detaching), and a `data:`
/// page (an opaque origin) covered by a link to a site;
/// when a person Cmd-clicks (Ctrl off macOS) it, and the page then fails to
/// load an address (an error page, of its own opaque origin) before the
/// eval runs;
/// then nothing opens.
#[test]
fn a_modified_click_does_not_open_from_an_error_page() {
    let Some((mut host, cache)) = spawn_with("modified-click-error", &[("GHOSTREALM_WEB_LINK_DELAY_MS", "3000")]) else {
        return;
    };
    let (a, _, hits) = serve_sites();
    let page = data_url(&format!(
        "<body style='margin:0'><a href='{a}/hit-error' style='position:fixed;inset:0;background:rgb(255,0,0)' \
         onclick=\"document.title = 'clicked'\">x</a>"
    ));
    assert!(host.send(&Request::Create { id: 1, url: page, width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);
    // No eval reaches the page before the link's, so its world is made
    // late; the page's title says it heard the click.
    let heard = || host.view(1).is_some_and(|v| v.title == "clicked");
    for _ in 0..5 {
        if heard() {
            break;
        }
        for (flags, action) in [
            (NEW_TAB, MouseAction::Move),
            (NEW_TAB | mods::LEFT_MOUSE, MouseAction::Down { button: MouseButton::Left, count: 1 }),
            (NEW_TAB, MouseAction::Up { button: MouseButton::Left, count: 1 }),
        ] {
            assert!(host.send(&Request::Mouse { id: 1, x: 20, y: 20, mods: flags, action }));
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        while !heard() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(heard(), "the page never heard a click");
    // A port nothing listens on.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    assert!(host.send(&Request::Navigate { id: 1, url: format!("http://{closed}/") }));
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.view(1).unwrap().error.is_none() {
        assert!(Instant::now() < deadline, "the load never failed");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(4000));
    assert!(host.take_popups(1).is_empty());
    assert!(!hits.lock().unwrap().iter().any(|h| h.path == "/hit-error"), "{:?}", hits.lock().unwrap());

    shut_down(host, &cache);
}

/// Given a page sandboxed by CSP (`allow-scripts allow-popups`) covered by
/// a link to another site;
/// when a person Cmd-clicks (Ctrl off macOS) it;
/// then the link opens in a tab sandboxed too (an opaque origin), as the
/// page's sandbox flags apply.
/// Given one sandboxed without `allow-popups`;
/// when a person Cmd-clicks its link;
/// then nothing opens and the page stays.
#[test]
fn a_modified_click_in_a_sandboxed_document_keeps_its_sandbox() {
    let Some((mut host, cache)) = spawn("modified-click-sandboxed") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    let open = |host: &mut WebHost, id, url: &str| {
        assert!(host.send(&Request::Create { id, url: url.into(), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
    };

    open(&mut host, 1, &format!("{b}/sandboxedlink?{a}/hit-sandboxed"));
    let tab = click_for_popup_with(&host, 1, NEW_TAB, MouseButton::Left);
    assert!(tab.from_input && tab.is_tab(), "{tab:?}");
    assert!(host.wait_loaded(tab.id, Duration::from_secs(20)), "the tab never loaded");
    assert_eq!(host.eval(tab.id, "self.origin", Duration::from_secs(5)), Ok("\"null\"".into()));

    let url = format!("{b}/sealedlink?{a}/hit-sealed");
    open(&mut host, 2, &url);
    click_at_until(&host, 2, (20, 20), NEW_TAB, "+(document.body.dataset.clicks || 0) > 0");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(host.take_popups(2).is_empty());
    assert!(!hits.lock().unwrap().iter().any(|h| h.path == "/hit-sealed"), "{:?}", hits.lock().unwrap());
    assert_eq!(host.view(2).unwrap().url, url);

    shut_down(host, &cache);
}

/// Given a local file (`file:`, an opaque origin) covered by a link to a
/// site;
/// when a person Cmd-clicks (Ctrl off macOS) it;
/// then the link opens in a tab, requested once.
#[test]
fn a_modified_click_in_a_local_file_opens_a_tab() {
    let Some((mut host, cache)) = spawn("modified-click-file") else {
        return;
    };
    let (a, _, hits) = serve_sites();
    let file = cache.with_extension("html");
    std::fs::write(&file, LINK.replace("LINK", &format!("href={a}/hit-file"))).unwrap();
    let url = format!("file://{}", file.display());
    assert!(host.send(&Request::Create { id: 1, url, width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);
    let tab = click_for_popup_with(&host, 1, NEW_TAB, MouseButton::Left);
    assert!(tab.from_input && tab.kind == PopupKind::BackgroundTab, "{tab:?}");
    assert_eq!(tab.url, format!("{a}/hit-file"));
    wait_hits(&hits, "/hit-file", 1);
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(wait_hits(&hits, "/hit-file", 1).len(), 1);

    shut_down(host, &cache);
    let _ = std::fs::remove_file(&file);
}

/// Given a host that runs a modified click's eval 1.5 s late, and a page
/// covered by a link to another site;
/// when a person Cmd-clicks (Ctrl off macOS) it, and the page then loads
/// its address again (a new document of the same origin, with the same
/// link) before the eval runs;
/// then nothing opens: the eval doesn't run in a document the click wasn't
/// in.
#[test]
fn a_modified_click_does_not_open_from_the_next_document() {
    let Some((mut host, cache)) = spawn_with("modified-click-gate", &[("GHOSTREALM_WEB_LINK_DELAY_MS", "1500")]) else {
        return;
    };
    let (a, b, hits) = serve_sites();
    let url = format!("{b}/link?{a}/hit-gated");
    assert!(host.send(&Request::Create { id: 1, url: url.clone(), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);
    click_at_until(&host, 1, (20, 20), NEW_TAB, "+(document.body.dataset.clicks || 0) > 0");
    host.expect_load(1);
    assert!(host.send(&Request::Navigate { id: 1, url }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "the page never loaded again");
    std::thread::sleep(Duration::from_millis(2500));
    assert!(host.take_popups(1).is_empty());
    assert!(hits.lock().unwrap().is_empty(), "{:?}", hits.lock().unwrap());

    shut_down(host, &cache);
}

/// Given a page filled by a `sandbox=allow-scripts` frame (no
/// `allow-popups`) holding a link;
/// when a person Cmd-clicks (Ctrl off macOS) the link;
/// then nothing opens and the page stays where it was.
#[test]
fn a_modified_click_in_a_frame_opens_nothing() {
    let Some((mut host, cache)) = spawn("modified-click-frame") else {
        return;
    };
    let (a, b, hits) = serve_sites();
    let url = format!("{b}/sandboxed?{a}/hit-framed");
    assert!(host.send(&Request::Create { id: 1, url: url.clone(), width: 100, height: 100, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);
    click_at_until(&host, 1, (20, 20), NEW_TAB, "+(document.body.dataset.clicks || 0) > 0");
    std::thread::sleep(Duration::from_millis(1000));
    assert!(host.take_popups(1).is_empty());
    assert!(hits.lock().unwrap().is_empty(), "{:?}", hits.lock().unwrap());
    assert_eq!(host.view(1).unwrap().url, url);

    shut_down(host, &cache);
}

/// A key press `kind` of Enter with `flags` held.
fn enter(kind: KeyKind, flags: u32) -> Key {
    // macOS's virtual key code for Return; Chromium elsewhere reads
    // `windows_key_code`.
    let native_key_code = if cfg!(target_os = "macos") { 0x24 } else { 0 };
    Key { kind, windows_key_code: 0x0D, native_key_code, character: 13, unmodified_character: 13, mods: flags }
}

/// Given a page with a form that posts;
/// when a person types in its field and presses Shift+Enter, and (on a
/// fresh copy) Shift-clicks its submit button;
/// then each posts the form in place, as on a plain submit, and no tab
/// opens.
#[test]
fn a_form_submitted_with_a_modifier_posts_in_place() {
    let Some((mut host, cache)) = spawn("modified-submit") else {
        return;
    };
    let (a, _, hits) = serve_sites();
    let open = |host: &mut WebHost, id| {
        assert!(host.send(&Request::Create { id, url: format!("{a}/post"), width: 100, height: 100, scale: 1.0 }));
        assert!(host.wait_loaded(id, Duration::from_secs(20)), "page never loaded");
        wait_painted(host, id, [0, 0, 255]);
    };

    open(&mut host, 1);
    click_at_until(&host, 1, (20, 80), 0, "document.activeElement.name == 'q'");
    for kind in [KeyKind::Down, KeyKind::Char, KeyKind::Up] {
        assert!(host.send(&Request::Key { id: 1, key: enter(kind, mods::SHIFT) }));
    }
    let [hit] = <[_; 1]>::try_from(wait_hits(&hits, "/hitpost", 1)).unwrap();
    assert_eq!(hit.method, "POST", "{hit:?}");
    assert!(host.wait_loaded(1, Duration::from_secs(20)));
    assert_eq!(host.view(1).unwrap().url, format!("{a}/hitpost"));

    open(&mut host, 2);
    let js = "+(document.body.dataset.clicks || 0) > 0 || location.pathname == '/hitpost'";
    click_at_until(&host, 2, (20, 20), mods::SHIFT, js);
    let got = wait_hits(&hits, "/hitpost", 2);
    assert_eq!(got[1].method, "POST", "{got:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while host.view(2).unwrap().url != format!("{a}/hitpost") {
        assert!(Instant::now() < deadline, "the form never posted in place");
        std::thread::sleep(Duration::from_millis(20));
    }
    std::thread::sleep(Duration::from_millis(500));
    assert!(host.take_popups(1).is_empty() && host.take_popups(2).is_empty());
    assert_eq!(hits.lock().unwrap().len(), 2, "{:?}", hits.lock().unwrap());

    shut_down(host, &cache);
}

#[test]
fn a_scripted_popup_is_drivable_and_closes_with_its_opener() {
    let Some((host, cache)) = spawn("popup-agent") else {
        return;
    };
    let (base, _) = serve();
    assert!(host.send(&Request::Create { id: 1, url: format!("{base}/opener"), width: 200, height: 150, scale: 1.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");

    // An eval is no user gesture unless asked; a gesture eval opens it.
    let blocked = host.eval(1, "window.open('/stay') !== null", Duration::from_secs(5));
    assert_eq!(blocked, Ok("false".into()));
    let activation = "navigator.userActivation.isActive";
    assert_eq!(host.eval(1, activation, Duration::from_secs(5)), Ok("false".into()));
    let opened = host.eval_as(1, "window.open('/stay') !== null", true, Duration::from_secs(5));
    assert_eq!(opened, Ok("true".into()));
    let [p] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert!(!p.from_input, "{p:?}");
    assert_eq!((p.width, p.height), (200, 150), "the opener's size");

    assert!(host.wait_loaded(p.id, Duration::from_secs(20)), "pop-up never loaded");
    assert_eq!(host.eval(p.id, "document.title", Duration::from_secs(5)), Ok("\"Stay\"".into()));
    assert!(host.text(p.id, Duration::from_secs(5)).unwrap().contains("stay text"));
    assert_eq!(host.view(p.id).unwrap().url, format!("{base}/stay"));
    assert_eq!(
        host.eval(p.id, "window.opener.postMessage('from the agent', '*'); 1", Duration::from_secs(5)),
        Ok("1".into())
    );
    assert_eq!(wait_got(&host, 1, "agent"), "\"from the agent;\"");

    // `window.close()` from an eval closes it.
    host.eval(p.id, "window.close()", Duration::from_secs(5)).ok();
    wait_closed(&host, p.id);

    // A page keeps at most `MAX_POPUPS` open; closing it closes them.
    let max = ghostrealm_web::protocol::MAX_POPUPS;
    let open = |n| host.eval_as(1, "window.open('/stay') !== null", true, Duration::from_secs(5)) == Ok(n);
    for _ in 0..max {
        assert!(open("true".into()));
    }
    assert!(open("false".into()), "one too many");
    assert_eq!(host.view(1).unwrap().popups_refused, 1);
    let open = wait_popups(&host, 1, max);
    assert!(host.send(&Request::Close { id: 1 }));
    wait_closed(&host, 1);
    for q in open {
        wait_closed(&host, q.id);
    }

    shut_down(host, &cache);
}

#[test]
fn an_eval_whose_page_navigates_away_runs_once() {
    let Some((host, cache)) = spawn("navigate") else {
        return;
    };
    let (base, hits) = serve();
    assert!(host.send(&Request::Create {
        id: 1,
        url: format!("{base}/a"),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    assert_eq!(host.eval(1, "document.title", Duration::from_secs(5)), Ok("\"A\"".into()));

    // The script leaves its document while it waits: it must not be run again
    // in the next one, nor answer from it.
    host.expect_load(1);
    let js = "(async () => { await fetch('/hit?' + document.title); \
              document.getElementById('l').click(); \
              await new Promise(r => setTimeout(r, 1500)); return document.title; })()";
    let result = host.eval(1, js, Duration::from_secs(10));
    assert!(result.is_err(), "{result:?}");
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page B never loaded");
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(*hits.lock().unwrap(), ["/hit?A"]);

    // The stale world is replaced on the next eval.
    assert_eq!(host.eval(1, "document.title", Duration::from_secs(5)), Ok("\"B\"".into()));

    shut_down(host, &cache);
}

#[test]
fn an_expired_eval_making_the_world_does_not_block_later_ones() {
    let Some((host, cache)) = spawn_with("ttl", &[("GHOSTREALM_WEB_EVAL_TTL_MS", "1000")]) else {
        return;
    };
    // Blocks its main thread for 3s, starting 300ms after loading.
    let page = "data:text/html,<title>Busy</title><script>setTimeout(() => \
                { const t = Date.now(); while (Date.now() - t < 3000); }, 300)</script>";
    assert!(host.send(&Request::Create {
        id: 1,
        url: page.into(),
        width: 64,
        height: 48,
        scale: 1.0
    }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    std::thread::sleep(Duration::from_millis(600));
    // Starts making the world, which the busy page doesn't answer in time.
    assert!(host.eval(1, "1", Duration::from_millis(300)).is_err());
    std::thread::sleep(Duration::from_millis(1200));
    // Past the TTL: the first eval is expired, and this one makes the world
    // once the page is free.
    assert_eq!(host.eval(1, "document.title", Duration::from_secs(10)), Ok("\"Busy\"".into()));

    shut_down(host, &cache);
}
