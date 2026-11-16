//! Observational measurements only. Build the opt-in core-bootstrap-harness,
//! then pass its path and sample count. Optional "contended" adds one CPU worker;
//! use taskset externally to share one CPU with that worker and the successor.
use std::{
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut args = std::env::args().skip(1);
    let binary = args.next().expect("harness path");
    let samples: usize = args.next().unwrap_or("20".into()).parse()?;
    let stop = Arc::new(AtomicBool::new(false));
    let load = (args.next().as_deref() == Some("contended")).then(|| {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                std::hint::black_box((0..1000).sum::<u64>());
            }
        })
    });
    eprintln!(
        "os={} arch={} cpus={} kernel={} load={}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::thread::available_parallelism()?.get(),
        std::fs::read_to_string("/proc/sys/kernel/osrelease")?.trim(),
        std::fs::read_to_string("/proc/loadavg")?.trim(),
    );
    for sample in 0..samples {
        let data = tempfile::tempdir()?;
        let sockets = test_socket_dir::SocketDir::new();
        let owners = test_socket_dir::SocketDir::new();
        std::fs::create_dir(data.path().join("project"))?;
        let output = Command::new(&binary)
            .arg("--config-dir")
            .arg(data.path().join("config"))
            .arg("--project")
            .arg(data.path().join("project"))
            .arg("--owners")
            .arg(owners.path())
            .arg("--ipc-parent")
            .arg(sockets.path())
            .args(["--scenario", "measure"])
            .output()?;
        if !output.status.success() {
            return Err(format!(
                "fixture failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )
            .into());
        }
        for line in String::from_utf8(output.stdout)?.lines() {
            let mut event: serde_json::Value = serde_json::from_str(line)?;
            if event["status"] == "ready" {
                event["sample"] = sample.into();
                println!("{event}");
            }
        }
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(load) = load {
        load.join().unwrap();
    }
    Ok(())
}
