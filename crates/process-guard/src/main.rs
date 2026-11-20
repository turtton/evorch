fn main() {
    #[cfg(target_os = "linux")]
    {
        // No implicit Result/panic diagnostics may contaminate target stdio.
        let code = match process_guard::guard::entry() {
            Ok(()) => 0,
            Err(_) => 125,
        };
        unsafe { libc::_exit(code) }
    }
    #[cfg(not(target_os = "linux"))]
    std::process::exit(125);
}
