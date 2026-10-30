//! `ghostrealm-web-host`: runs Chromium (CEF) off-screen for ghostrealm's web
//! surfaces. Reads [`Request`]s from stdin and writes [`Event`]s to stdout, one
//! JSON object per line; page pixels go through shared-memory frame files (see
//! `ghostrealm_web::frame`). Exits when stdin closes.
//!
//! [`Request`]: ghostrealm_web::protocol::Request
//! [`Event`]: ghostrealm_web::protocol::Event

mod browser;
#[cfg(target_os = "macos")]
mod mac;

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Mutex, OnceLock};

use cef::*;
use ghostrealm_web::protocol::{Event, Request, PROTOCOL_VERSION};

/// Overrides the profile directory (cookies, cache, local storage).
const CACHE_ENV: &str = "GHOSTREALM_WEB_CACHE";

/// How long closing the browsers may take after the app hangs up before the
/// host exits anyway, so a stalled CEF shutdown never leaves it running.
const SHUTDOWN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

static STDOUT: Mutex<()> = Mutex::new(());

/// Where frame files go ([`ghostrealm_web::frame::frame_dir`]), set before CEF
/// starts.
static FRAME_DIR: OnceLock<PathBuf> = OnceLock::new();

/// The frame file directory.
pub fn frame_dir() -> &'static Path {
    FRAME_DIR.get().expect("frame dir is set before CEF starts")
}

/// Write one event line to stdout.
pub fn emit(event: &Event) {
    let Ok(mut line) = serde_json::to_vec(event) else {
        return;
    };
    line.push(b'\n');
    let _guard = STDOUT.lock();
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(&line).and_then(|_| out.flush());
}

/// Report why the host can't run, to the app and to stderr.
fn fail(error: &str) -> ExitCode {
    eprintln!("ghostrealm-web-host: {error}");
    emit(&Event::Failed {
        error: error.to_string(),
    });
    ExitCode::FAILURE
}

/// Whether process `pid` exists.
fn running(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: signal 0 only checks that the process exists.
        let r = unsafe { libc::kill(pid as libc::pid_t, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true
    }
}

/// Profile directory: `$GHOSTREALM_WEB_CACHE`, else the platform cache dir.
fn cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os(CACHE_ENV) {
        return PathBuf::from(p);
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    let base = if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".cache"))
    };
    base.join("ghostrealm").join("web")
}

/// Point Chromium's temp dir at one of this host's own ([`HostTemp`]), unless
/// this is one of Chromium's subprocesses (`--type=…`; off macOS this binary
/// serves them), which inherit it. Without one Chromium uses the system temp
/// dir.
///
/// [`HostTemp`]: ghostrealm_web::temp::HostTemp
fn chromium_temp(temp: &Path) -> Option<ghostrealm_web::temp::HostTemp> {
    if std::env::args_os().any(|a| a.to_string_lossy().starts_with("--type=")) {
        return None;
    }
    match ghostrealm_web::temp::HostTemp::create(temp, running) {
        Ok(temp) => {
            std::env::set_var(ghostrealm_web::temp::ENV, temp.path());
            Some(temp)
        }
        Err(e) => {
            eprintln!("ghostrealm-web-host: Chromium's temp files go in the system temp dir: {e}");
            None
        }
    }
}

fn main() -> ExitCode {
    // The system temp dir, read before `chromium_temp` changes `TMPDIR` (off
    // macOS) for Chromium.
    let temp = std::env::temp_dir();
    // First, while this is the only thread: changing the environment races
    // with other threads reading it, and loading CEF starts some. Dropped
    // last, after CEF has shut down.
    let _chromium_temp = chromium_temp(&temp);
    #[cfg(target_os = "macos")]
    let _loader = {
        let exe = std::env::current_exe().expect("current exe");
        let loader = library_loader::LibraryLoader::new(&exe, false);
        if !loader.load() {
            return fail("cannot load the CEF framework (run the host from its app bundle)");
        }
        loader
    };
    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);
    #[cfg(target_os = "macos")]
    mac::setup_application();

    let args = args::Args::new();
    let mut app = HostApp::new();
    // Off macOS this binary also serves Chromium's subprocesses.
    let code = execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    if code >= 0 {
        return ExitCode::from(code as u8);
    }

    let frames = match ghostrealm_web::frame::frame_dir_from(&temp) {
        Ok(d) => d,
        Err(e) => return fail(&format!("no directory for page frames: {e}")),
    };
    ghostrealm_web::frame::remove_stale(&frames, running);
    ghostrealm_web::frame::remove_stale_dirs(&temp, running);
    let _ = FRAME_DIR.set(frames);
    let cache = cache_dir();
    let _ = std::fs::create_dir_all(&cache);
    // CEF resolves symlinks in `root_cache_path` but not in `cache_path`, and
    // keeps the profile in memory when it then finds `cache_path` outside the
    // root (as under macOS's temp dir, `/var` -> `/private/var`).
    let cache = std::fs::canonicalize(&cache).unwrap_or(cache);
    let settings = Settings {
        windowless_rendering_enabled: 1,
        root_cache_path: CefString::from(cache.to_string_lossy().as_ref()),
        cache_path: CefString::from(cache.join("profile").to_string_lossy().as_ref()),
        persist_session_cookies: 1,
        log_file: CefString::from(cache.join("host.log").to_string_lossy().as_ref()),
        log_severity: LogSeverity::WARNING,
        background_color: 0xFFFF_FFFF,
        ..Default::default()
    };
    if initialize(
        Some(args.as_main_args()),
        Some(&settings),
        Some(&mut app),
        std::ptr::null_mut(),
    ) != 1
    {
        let code = get_exit_code();
        let in_use = [
            sys::cef_resultcode_t::CEF_RESULT_CODE_PROFILE_IN_USE,
            sys::cef_resultcode_t::CEF_RESULT_CODE_NORMAL_EXIT_PROCESS_NOTIFIED,
        ]
        .iter()
        .any(|c| *c as i32 == code);
        return fail(&if in_use {
            format!(
                "the web profile {} is in use by another ghostrealm process",
                cache.display()
            )
        } else {
            format!("CEF failed to initialise (exit code {code})")
        });
    }
    run_message_loop();
    shutdown();
    ExitCode::SUCCESS
}

/// Forward stdin requests to the UI thread; EOF means the app hung up.
fn serve_stdin() {
    std::thread::Builder::new()
        .name("requests".into())
        .spawn(|| {
            for line in std::io::stdin().lock().lines() {
                let Ok(line) = line else { break };
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<Request>(&line) {
                    Ok(req) => {
                        post_task(ThreadId::UI, Some(&mut RequestTask::new(Some(req))));
                    }
                    Err(e) => eprintln!("ghostrealm-web-host: bad request: {e}"),
                }
            }
            post_task(ThreadId::UI, Some(&mut RequestTask::new(None)));
            std::thread::sleep(SHUTDOWN_GRACE);
            eprintln!("ghostrealm-web-host: shutdown stalled; exiting");
            // CEF may still be shutting down on the UI thread: `exit` would run
            // atexit handlers and static destructors under it.
            #[cfg(unix)]
            // SAFETY: `_exit` only ends the process.
            unsafe {
                libc::_exit(0)
            };
            #[cfg(not(unix))]
            std::process::exit(0);
        })
        .expect("spawn request reader");
}

wrap_task! {
    struct RequestTask {
        req: Option<Request>,
    }

    impl Task {
        fn execute(&self) {
            browser::handle(self.req.clone());
        }
    }
}

wrap_app! {
    struct HostApp;

    impl App {
        fn on_before_command_line_processing(
            &self,
            _process_type: Option<&CefStringUtf16>,
            command_line: Option<&mut CommandLine>,
        ) {
            let Some(command_line) = command_line else { return };
            // Keep Chromium from prompting for (or writing to) the login keychain.
            command_line.append_switch(Some(&"use-mock-keychain".into()));
            // Pages reach the app as CPU pixels (`on_paint`), so composite in
            // software: GPU compositing would add a readback and fails to paint
            // at all when no window server is available.
            command_line.append_switch(Some(&"disable-gpu-compositing".into()));
        }

        fn browser_process_handler(&self) -> Option<BrowserProcessHandler> {
            Some(HostProcessHandler::new())
        }
    }
}

wrap_browser_process_handler! {
    struct HostProcessHandler;

    impl BrowserProcessHandler {
        fn on_context_initialized(&self) {
            emit(&Event::Ready {
                protocol: PROTOCOL_VERSION,
            });
            serve_stdin();
        }

        /// Another host started on this profile. Handling it stops CEF opening
        /// a browser window of its own here (which nothing would ever close);
        /// the other host reports the profile in use and exits.
        fn on_already_running_app_relaunch(
            &self,
            _command_line: Option<&mut CommandLine>,
            _current_directory: Option<&CefString>,
        ) -> std::os::raw::c_int {
            1
        }
    }
}
