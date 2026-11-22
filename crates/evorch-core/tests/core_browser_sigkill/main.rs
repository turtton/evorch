//! B3-08: real Chromium and real CEF under a real, sealed `evorch-core`
//! (Linux). A dedicated harness=false leaf; see README.md for the evidence
//! classification (P* only) and the bounds.
#[cfg(target_os = "linux")]
mod cells;
#[cfg(target_os = "linux")]
mod core_process;
#[cfg(target_os = "linux")]
mod evidence;
#[cfg(target_os = "linux")]
mod fixtures;
#[cfg(target_os = "linux")]
#[path = "../../shared-test-support/kill_observer/mod.rs"]
mod observer;
#[cfg(target_os = "linux")]
mod procs;
#[cfg(target_os = "linux")]
mod tls;

#[cfg(target_os = "linux")]
const CELLS: &[&str] = &["chromium_p", "chromium_stopped_p", "cef_p", "cef_stopped_p"];

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("core browser SIGKILL cells: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {}

/// The cells need a real browser (and CEF), so they are listed like
/// `#[ignore]`d tests: only `--ignored` (nextest `--run-ignored only`) lists
/// them, and the ordinary groups never run them. A missing browser, CEF or
/// certutil FAILS the cell; it is never skipped.
#[cfg(target_os = "linux")]
fn run() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.iter().any(|arg| arg == "--list") {
        if args.iter().any(|arg| arg == "--ignored") {
            for name in CELLS {
                println!("{name}: test");
            }
        }
        return Ok(());
    }
    let selected: Vec<_> = args
        .iter()
        .filter(|arg| CELLS.contains(&arg.as_str()))
        .collect();
    if selected.is_empty() {
        if !args.iter().any(|arg| arg == "--ignored") {
            return Ok(());
        }
        for name in CELLS {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args(["--ignored", "--exact", name])
                .status()?;
            if !status.success() {
                return Err(std::io::Error::other(format!("{name}: {status}")));
            }
        }
        return Ok(());
    }
    observer::ProcessTree::enable_controller_subreaper()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    // Whole-cell failure watchdog only; every individual wait is bounded by
    // 30 s and success always comes from an explicit event.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(observer::bounded(
            std::time::Duration::from_secs(120),
            "whole-cell failure watchdog",
            cells::run(selected[0]),
        ))
    }));
    let cleanup = runtime.block_on(observer::ProcessTree::cleanup_controller(
        std::time::Duration::from_secs(30),
    ));
    // A blocking provider-gate thread may outlive a failed cell; never wait
    // for it without a bound.
    runtime.shutdown_timeout(std::time::Duration::from_secs(5));
    match result {
        Ok(result) => result.and(cleanup),
        Err(_) => {
            cleanup?;
            Err(std::io::Error::other(
                "cell panicked; controller reclaimed descendants",
            ))
        }
    }
}
