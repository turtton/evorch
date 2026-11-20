//! Single-thread raw-handoff composition. Only this test target owns fixture
//! argv; the feature-free product has no clock/fault injection interface.
#[cfg(target_os = "linux")]
mod composition;
#[cfg(target_os = "linux")]
mod parent;
#[cfg(target_os = "linux")]
const CELLS: &[&str] = &[
    "cold_flush",
    "planned_flush",
    "cold_final_usage",
    "cold_writer_barrier",
    "cold_executor",
    "cold_host_lock",
    "cold_stopped_join",
    "cold_control_web",
    "cold_cleanup_returns",
    "product_exec",
    "product_planned_idle",
    "product_after_expiry",
];

#[cfg(target_os = "linux")]
fn main() {
    if let Err(error) = run() {
        eprintln!("watchdog composition: {error}");
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
        return composition::run(args);
    }
    let mut list = false;
    let mut ignored = false;
    let mut name = None;
    let mut exact = false;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.to_str() {
            Some("--list") => list = true,
            Some("--ignored") => ignored = true,
            Some("--include-ignored" | "--nocapture") => (),
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
    if list {
        if exact || name.is_some() {
            std::process::exit(2);
        }
        if !ignored {
            for cell in CELLS {
                println!("{cell}: test");
            }
        }
        return Ok(());
    }
    if let Some(name) = name {
        if !exact || !CELLS.contains(&name.as_str()) {
            eprintln!("unknown/nonexact watchdog cell: {name}");
            std::process::exit(2);
        }
        if ignored {
            return Ok(());
        }
        // No signal mask, runtime, or thread is installed in this cell parent.
        return parent::run(&name);
    }
    if exact {
        std::process::exit(2);
    }
    if ignored {
        return Ok(());
    }
    for cell in CELLS {
        let status = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", cell, "--nocapture"])
            .status()?;
        if !status.success() {
            return Err(format!("{cell}: {status}").into());
        }
    }
    Ok(())
}
