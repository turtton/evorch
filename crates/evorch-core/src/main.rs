use evorch_core::{
    backend::HostBackend,
    instance::{Instance, InstanceError},
    lifecycle::IdleGrace,
    unix,
};
use std::{path::PathBuf, sync::Arc, time::Duration};
use workbench_host::bootstrap::{BootstrapError, BootstrapOptions, BootstrappedHost};
use workbench_web::{WebServer, WebServerConfig};

const USAGE: &str = "usage: evorch-core [--config-dir <dir>] [--web] [--foreground]";
#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            let code = if e
                .downcast_ref::<InstanceError>()
                .is_some_and(|e| matches!(e, InstanceError::AlreadyRunning))
            {
                2
            } else {
                1
            };
            eprintln!(
                "evorch-core: {}",
                secret_guard::SecretRedactor::from_env()
                    .redact(&e.to_string())
                    .text
            );
            std::process::ExitCode::from(code)
        }
    }
}
async fn run() -> Result<(), BootstrapError> {
    let mut config_dir = config::user_config_dir();
    let mut web = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config-dir" => config_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            "--web" => web = true,
            "--foreground" => (), // P7a always foreground; no daemonisation or GUI autostart.
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let config_dir = config_dir.ok_or("cannot resolve config directory; pass --config-dir")?;
    let instance = Instance::acquire_default(&config_dir)?;
    let config_dir = std::fs::canonicalize(config_dir)?;
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".local/state")))
        .ok_or("cannot resolve ownership state directory")?
        .join("evorch");
    // Same owner registry as the unchanged GUI (ADR 0024).
    let mut host = BootstrappedHost::open(BootstrapOptions {
        config_dir,
        ownership_root: state_dir.join("threads"),
        project_root: std::env::current_dir()?,
    })
    .await?;
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    let web_config = WebServerConfig {
        enabled: web || host.config.web.enabled,
        bind: ([127, 0, 0, 1], host.config.web.port).into(),
        allowed_hosts: host.config.web.allowed_hosts.clone(),
        allowed_origins: host.config.web.allowed_origins.clone(),
        ..Default::default()
    };
    let web_enabled = web_config.enabled;
    backend.set_web_enabled(web_enabled);
    let server = match WebServer::start(web_config, backend.clone()).await {
        Ok(s) => s,
        Err(e) => {
            host.shutdown().await?;
            return Err(e.into());
        }
    };
    let listener = match instance.bind() {
        Ok(l) => l,
        Err(e) => {
            if let Some(s) = server {
                s.shutdown().await?;
            }
            host.shutdown().await?;
            return Err(e.into());
        }
    };
    // Explicit readiness AFTER both listeners bind. stdout is machine-readable.
    println!(
        "{}",
        serde_json::json!({"status":"ready", "socket":instance.socket, "core":instance.identity, "web":server.as_ref().map(|s| s.local_addr().to_string())})
    );
    let (shutdown, _) = tokio::sync::watch::channel(false);
    let mut clients = tokio::task::JoinSet::new();
    let mut events = host.runtime.event_bus().subscribe();
    let background = host.host.lock().unwrap().service().background_work_notify();
    let mut idle = IdleGrace::new(Duration::from_secs(host.config.core.idle_grace_secs));
    let outcome = loop {
        host.host.lock().unwrap().service_mut().poll();
        let deadline = idle.update(web_enabled, clients.len(), host.busy());
        let expired = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = terminate.recv() => break Ok(()),
            _ = interrupt.recv() => break Ok(()),
            _ = background.notified() => (),
            _ = expired => {
                if !host.busy() { break Ok(()); }
            },
            event = events.recv() => {
                if let Ok(event) = event {
                    host.host.lock().unwrap().service_mut().observe_lifecycle(&event);
                }
            },
            _ = clients.join_next(), if !clients.is_empty() => (),
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    if clients.len() < 64 {
                        clients.spawn(unix::connection(stream, backend.clone(), shutdown.subscribe()));
                    }
                },
                Err(e) => break Err(e),
            },
        }
    };
    // No ingress can start new work from here; close transports before drain.
    drop(listener);
    backend.close();
    let _ = shutdown.send(true);
    let web_result = match server {
        Some(s) => s.shutdown().await,
        None => Ok(()),
    };
    while clients.join_next().await.is_some() {}
    host.shutdown().await?;
    drop(backend);
    drop(host); // storage leases before the lifetime lock
    drop(instance);
    web_result?;
    outcome?;
    Ok(())
}
