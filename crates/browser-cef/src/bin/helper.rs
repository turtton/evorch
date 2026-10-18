//! CEF subprocess entry point (renderer, GPU, utility). Installed next to
//! `evorch-gui`, which passes its path as `browser_subprocess_path`.

fn main() {
    let _ = cef::api_hash(cef::sys::CEF_API_VERSION_LAST, 0);
    let args = cef::args::Args::new();
    let mut app = browser_cef::subprocess_app();
    let code = cef::execute_process(
        Some(args.as_main_args()),
        Some(&mut app),
        std::ptr::null_mut(),
    );
    std::process::exit(code.max(0));
}
