#[cfg(target_os = "linux")]
mod entries;
#[cfg(target_os = "linux")]
mod evaluation;
#[cfg(target_os = "linux")]
mod evidence;
#[cfg(target_os = "linux")]
mod fixtures;
#[cfg(target_os = "linux")]
#[path = "../../shared-test-support/kill_observer/mod.rs"]
mod observer;
#[cfg(target_os = "linux")]
mod observer_cells;
#[cfg(target_os = "linux")]
mod peer;
#[cfg(target_os = "linux")]
mod product;
#[cfg(target_os = "linux")]
mod t_controller;
#[cfg(target_os = "linux")]
mod test_callers;
#[cfg(target_os = "linux")]
mod trace;

#[cfg(target_os = "linux")]
const CELLS: &[&str] = &[
    "observer",
    "observer_escaped",
    "observer_stale_ancestor",
    "observer_trace_thread",
    "shell",
    "shell_pty",
    "grep",
    "git_diff",
    "git_read",
    "bwrap_probe",
    "comment_checker",
    "mcp",
    "js_worker",
    "libraries",
    "pull_request",
    "eval",
    "shell_direct_t",
    "js_direct_t",
    "lsp_t",
    "quota_t",
    "legacy_stdio_t",
    "probe_t",
    "renderer_t",
    "chromium_t",
];

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("core SIGKILL matrix: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn run() -> std::io::Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|arg| arg == "--peer") {
        return peer::run(&args[1..]);
    }
    if args.first().is_some_and(|arg| arg == "--thread-fixture") {
        return trace::thread_fixture(&args[1]);
    }
    if args.first().is_some_and(|arg| arg == "--t-owner") {
        return tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()?
            .block_on(test_callers::owner(
                &args[1],
                std::path::Path::new(&args[2]),
                std::path::Path::new(&args[3]),
                std::path::Path::new(&args[4]),
            ));
    }
    if args.iter().any(|arg| arg == "--list") {
        if !args.iter().any(|arg| arg == "--ignored") {
            for name in CELLS {
                println!("{name}: test");
            }
        }
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--ignored") {
        return Ok(());
    }
    let selected: Vec<_> = args
        .iter()
        .filter(|arg| CELLS.contains(&arg.as_str()))
        .collect();
    if selected.is_empty() {
        for name in CELLS {
            let status = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", name])
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
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        runtime.block_on(observer::bounded(
            std::time::Duration::from_secs(120),
            "whole-cell failure watchdog",
            product::run(selected[0]),
        ))
    }));
    let cleanup = runtime.block_on(observer::ProcessTree::cleanup_controller(
        std::time::Duration::from_secs(30),
    ));
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
