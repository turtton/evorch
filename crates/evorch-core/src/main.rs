use evorch_core::{
    backend::HostBackend,
    instance::{ConfigIdentity, Instance, InstanceError, LaunchGuard},
    lifecycle::{LifecycleDriver, OperatorSignals},
    recovery_service::CoreGraphRecovery,
};
use std::{path::PathBuf, sync::Arc};
use workbench_host::bootstrap::{BootstrapError, BootstrapOptions, BootstrappedHost};
use workbench_web::{WebServer, WebServerConfig};

const USAGE: &str = "usage: evorch-core [--config-dir <dir>] [--web] [--foreground]";
fn main() -> std::process::ExitCode {
    match run() {
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
fn run() -> Result<(), BootstrapError> {
    let mut config_dir = config::user_config_dir();
    let mut web = false;
    let mut launch_fd = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config-dir" => config_dir = Some(PathBuf::from(args.next().ok_or(USAGE)?)),
            "--web" => web = true,
            "--launch-fd" if launch_fd.is_none() => {
                launch_fd = Some(args.next().ok_or(USAGE)?.parse::<i32>()?)
            }
            "--foreground" => (), // P7a always foreground; no daemonisation or GUI autostart.
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            _ => return Err(USAGE.into()),
        }
    }
    let config_dir = config_dir.ok_or("cannot resolve config directory; pass --config-dir")?;
    let identity = ConfigIdentity::new(&config_dir)?;
    let launch = match launch_fd {
        // SAFETY: parsed once at entry, before the Tokio runtime/other threads;
        // inherited descriptor ownership is transferred and validated by adopt.
        Some(fd) => unsafe { LaunchGuard::adopt(&identity, fd)? },
        None => LaunchGuard::acquire(&identity)?,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(identity.config_dir().to_path_buf(), web, launch))
}
async fn serve(config_dir: PathBuf, web: bool, launch: LaunchGuard) -> Result<(), BootstrapError> {
    let instance = Instance::acquire_default(&launch)?;
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".local/state")))
        .ok_or("cannot resolve ownership state directory")?
        .join("evorch");
    // Same owner registry as the unchanged GUI (ADR 0024).
    let mut host = BootstrappedHost::open_for_core(
        BootstrapOptions {
            config_dir,
            ownership_root: state_dir.join("threads"),
            project_root: std::env::current_dir()?,
        },
        &CoreGraphRecovery,
    )
    .await?;
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let signals = OperatorSignals::new()?;
    let mut lifecycle = LifecycleDriver::new(&host);
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
    let listener = match instance.bind().and_then(|l| {
        l.set_nonblocking(true)?;
        tokio::net::UnixListener::from_std(l)
    }) {
        Ok(l) => l,
        Err(e) => {
            if let Some(s) = server {
                s.shutdown().await?;
            }
            host.shutdown().await?;
            return Err(e.into());
        }
    };
    if let Err(e) = instance.publish(backend.hello().instance_epoch.0) {
        if let Some(s) = server {
            s.shutdown().await?;
        }
        host.shutdown().await?;
        return Err(e.into());
    }
    drop(launch); // close only; never unlock the parent/child shared OFD
    // Explicit readiness AFTER both listeners bind. stdout is machine-readable.
    println!(
        "{}",
        serde_json::json!({"status":"ready", "socket":instance.socket, "core":instance.identity, "web":server.as_ref().map(|s| s.local_addr().to_string())})
    );
    let outcome = lifecycle
        .run(&host, backend.clone(), listener, web_enabled, signals)
        .await;
    let web_result = match server {
        Some(s) => s.shutdown().await,
        None => Ok(()),
    };
    host.shutdown().await?;
    drop(backend);
    drop(host); // storage leases before the lifetime lock
    drop(instance);
    web_result?;
    outcome?;
    Ok(())
}
