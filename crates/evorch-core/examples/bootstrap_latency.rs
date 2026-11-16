//! Baseline for the ordinary, unconfigured production bootstrap. Measurements
//! are observations, never test deadlines. Use fresh directories for each sample.
use std::time::Instant;
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let samples: usize = std::env::args().nth(1).unwrap_or("20".into()).parse()?;
    eprintln!(
        "os={} arch={} cpus={} profile={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()?.get(),
        if cfg!(debug_assertions) {
            "dev"
        } else {
            "release"
        },
    );
    eprintln!(
        "kernel={}",
        std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim()
    );
    eprintln!("load={}", std::fs::read_to_string("/proc/loadavg")?.trim());
    println!("sample,bootstrap_ms,shutdown_ms");
    for sample in 0..samples {
        let directory = tempfile::tempdir()?;
        let config = directory.path().join("config");
        let root = directory.path().join("project");
        std::fs::create_dir_all(&config)?;
        std::fs::create_dir_all(&root)?;
        let started = Instant::now();
        let mut host = BootstrappedHost::open_for_core(
            BootstrapOptions {
                config_dir: config,
                ownership_root: directory.path().join("owners"),
                project_root: root,
            },
            &evorch_core::recovery_service::CoreGraphRecovery,
        )
        .await?;
        let bootstrap = started.elapsed();
        let stopped = Instant::now();
        host.shutdown().await?;
        println!(
            "{sample},{:.3},{:.3}",
            bootstrap.as_secs_f64() * 1000.,
            stopped.elapsed().as_secs_f64() * 1000.,
        );
    }
    Ok(())
}
