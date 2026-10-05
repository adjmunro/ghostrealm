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
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;

use cef::*;
use ghostrealm_web::protocol::{Event, Request};

/// Overrides the profile directory (cookies, cache, local storage).
const CACHE_ENV: &str = "GHOSTREALM_WEB_CACHE";

static STDOUT: Mutex<()> = Mutex::new(());

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

fn main() -> ExitCode {
    #[cfg(target_os = "macos")]
    let _loader = {
        let exe = std::env::current_exe().expect("current exe");
        let loader = library_loader::LibraryLoader::new(&exe, false);
        if !loader.load() {
            eprintln!(
                "ghostrealm-web-host: cannot load the CEF framework (run from the app bundle)"
            );
            return ExitCode::FAILURE;
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

    let cache = cache_dir();
    let _ = std::fs::create_dir_all(&cache);
    let settings = Settings {
        no_sandbox: 1,
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
        eprintln!("ghostrealm-web-host: CEF failed to initialise");
        return ExitCode::FAILURE;
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
            emit(&Event::Ready);
            serve_stdin();
        }
    }
}
