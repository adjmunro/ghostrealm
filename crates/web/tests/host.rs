//! End-to-end against the real CEF host: a page loads, paints the expected
//! pixels into the shared frame, answers text/eval requests (which a hostile
//! page can't tamper with), and hears each mouse button.
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
}

/// A local HTTP server: `/a` and `/b` are pages (each links to the other);
/// `/opener` opens `/popup` on a click (red while it waits for one) and logs
/// messages it gets in `data-got`; `/popup` posts to its opener and closes
/// itself; `/stay` is a page. `/hit...` paths are recorded in the returned
/// list. Anything else is empty.
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
(document.body.dataset.got || '') + e.data + (e.source ? '' : ' (no source)') + ';'; });</script>";

/// Wait for page `id`'s first painted frame whose top-left pixel is `bgr`.
fn wait_painted(host: &mut WebHost, id: u64, bgr: [u8; 3]) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !host.frame(id).and_then(|f| f.read()).is_some_and(|px| px[..3] == bgr) {
        assert!(Instant::now() < deadline, "page {id} never painted");
        std::thread::sleep(Duration::from_millis(50));
    }
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
    assert!(host.send(&Request::Create { id: 1, url: format!("{base}/opener"), width: 200, height: 150, scale: 2.0 }));
    assert!(host.wait_loaded(1, Duration::from_secs(20)), "page never loaded");
    wait_painted(&mut host, 1, [0, 0, 255]);

    let click = |action| assert!(host.send(&Request::Mouse { id: 1, x: 20, y: 20, mods: 0, action }));
    click(MouseAction::Move);
    click(MouseAction::Down { button: MouseButton::Left, count: 1 });
    click(MouseAction::Up { button: MouseButton::Left, count: 1 });
    let [p] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert!(p.id >= ghostrealm_web::protocol::POPUP_IDS, "{p:?}");
    assert!(p.from_input, "a click opened it");
    assert!(p.url.ends_with("/popup"), "{p:?}");
    assert_eq!((p.width, p.height), (600, 400), "the size it asked for, in physical pixels");

    // It posts to its opener, then closes itself.
    wait_closed(&host, p.id);
    let got = wait_got(&host, 1, "from /popup");
    assert_eq!(got, "\"from /popup;\"");
    assert!(!host.is_closed(1), "the opener stays");

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    // Closing the opener closes the pop-ups it still has.
    assert_eq!(host.eval_as(1, "window.open('/stay') !== null", true, Duration::from_secs(5)), Ok("true".into()));
    let [q] = <[_; 1]>::try_from(wait_popups(&host, 1, 1)).unwrap();
    assert!(host.send(&Request::Close { id: 1 }));
    wait_closed(&host, 1);
    wait_closed(&host, q.id);

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
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

    drop(host);
    let _ = std::fs::remove_dir_all(&cache);
}
