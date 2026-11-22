//! B3-08b: real Chromium and real CEF, navigation complete, then the real
//! ten-second planned-shutdown expiry (exit 74) and reaping of the owned
//! children. Release-profile, opt-in (ignored) harness=false leaf; see
//! README.md for what this does and does not establish (class T).
#[cfg(target_os = "linux")]
#[path = "../planned_shutdown_watchdog_composition/composition.rs"]
mod composition;
#[cfg(target_os = "linux")]
mod controller;
#[cfg(target_os = "linux")]
mod fixture;
#[cfg(target_os = "linux")]
#[path = "../core_browser_sigkill/fixtures.rs"]
#[allow(dead_code)]
mod fixtures;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../shared-test-support/kill_observer/mod.rs"]
mod observer;
#[cfg(target_os = "linux")]
#[path = "../core_browser_sigkill/procs.rs"]
#[allow(dead_code)]
mod procs;
#[cfg(target_os = "linux")]
#[path = "../core_browser_sigkill/tls.rs"]
#[allow(dead_code)]
mod tls;

#[cfg(target_os = "linux")]
const CELLS: &[&str] = &[
    "expiry_chromium_t",
    "expiry_chromium_stopped_t",
    "expiry_cef_t",
    "expiry_cef_stopped_t",
];

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("core browser expiry: {error}");
        std::process::exit(1);
    }
}
#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn run() -> Result<(), workbench_host::bootstrap::BootstrapError> {
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    if args == ["--describe-handoff"] {
        println!(
            "{}",
            serde_json::to_string(&evorch_core::handoff_description::describe()?)?
        );
        return Ok(());
    }
    if args.iter().any(|arg| arg == "--watchdog-child") {
        composition::OWNED_FIXTURE
            .set(fixture::start)
            .map_err(|_| "owned fixture registered twice")?;
        return composition::run(args);
    }
    let (mut list, mut ignored, mut exact, mut name) = (false, false, false, None);
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--list") => list = true,
            Some("--ignored" | "--include-ignored") => ignored = true,
            Some("--nocapture" | "--no-capture") => (),
            Some("--exact") => exact = true,
            Some("--format") if args.next().as_deref() == Some(std::ffi::OsStr::new("terse")) => (),
            Some(value) if !value.starts_with('-') && name.is_none() => {
                name = Some(value.to_owned())
            }
            _ => {
                eprintln!("unsupported harness argument: {arg:?}");
                std::process::exit(2);
            }
        }
    }
    // Every cell is an ignored test that CI selects explicitly; a missing
    // browser, CEF or certutil FAILS the cell, it is never skipped.
    if list {
        if ignored {
            for cell in CELLS {
                println!("{cell}: test");
            }
        }
        return Ok(());
    }
    if let Some(name) = name {
        if !exact || !CELLS.contains(&name.as_str()) {
            eprintln!("unknown/nonexact core browser expiry cell: {name}");
            std::process::exit(2);
        }
        if !ignored {
            return Ok(());
        }
        return Ok(controller::run_cell(&name)?);
    }
    if !ignored {
        return Ok(());
    }
    for cell in CELLS {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", cell, "--ignored", "--nocapture"])
            .status()?;
        if !status.success() {
            return Err(format!("{cell}: {status}").into());
        }
    }
    Ok(())
}
