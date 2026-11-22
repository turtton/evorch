//! B3-07: real ten-second planned-shutdown expiry, exit 74 and reaping of
//! owned children. Release-profile, opt-in (ignored) harness=false leaf; see
//! README.md for what this does and does not establish.
#[cfg(target_os = "linux")]
#[path = "../planned_shutdown_watchdog_composition/composition.rs"]
mod composition;
#[cfg(target_os = "linux")]
mod controller;
#[cfg(target_os = "linux")]
#[allow(dead_code)]
#[path = "../../shared-test-support/kill_observer/mod.rs"]
mod observer;
#[cfg(target_os = "linux")]
#[path = "../core_sigkill/peer.rs"]
mod peer;

#[cfg(target_os = "linux")]
const CELLS: &[&str] = &["expiry_capture_first", "expiry_signal_first"];

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("core expiry: {error}");
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
    if args.first().is_some_and(|arg| arg == "--peer") {
        let args: Vec<String> = args
            .iter()
            .skip(1)
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        return Ok(peer::run(&args)?);
    }
    if args.iter().any(|arg| arg == "--watchdog-child") {
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
    // Release-only, never in the default (debug) inventory: every cell is an
    // ignored test that CI selects explicitly with --run-ignored only.
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
            eprintln!("unknown/nonexact core expiry cell: {name}");
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
