//! Chromium subprocess entry (GPU, renderer, utility, ...) for the macOS host
//! bundle, where each runs from a helper app inside `Contents/Frameworks`.

fn main() -> std::process::ExitCode {
    #[cfg(target_os = "macos")]
    let _loader = {
        let exe = std::env::current_exe().expect("current exe");
        let loader = cef::library_loader::LibraryLoader::new(&exe, true);
        if !loader.load() {
            return std::process::ExitCode::FAILURE;
        }
        loader
    };
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let args = cef::args::Args::new();
    let code = cef::execute_process(
        Some(args.as_main_args()),
        None::<&mut cef::App>,
        std::ptr::null_mut(),
    );
    std::process::ExitCode::from(code.max(0) as u8)
}
