use evorch_core::{
    backend::HostBackend,
    instance::{ConfigIdentity, Instance, InstanceError, LaunchGuard},
    lifecycle::{LifecycleDriver, OperatorSignals},
    recovery_service::CoreGraphRecovery,
};
use std::{path::PathBuf, sync::Arc};
use workbench_host::bootstrap::{BootstrapError, BootstrapOptions, BootstrappedHost};
use workbench_web::{WebServer, WebServerConfig};

mod args;
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
    #[cfg(target_os = "linux")]
    let mask = core_instance::handoff::BlockedShutdownSignals::at_entry()?;
    let args = args::parse(std::env::args().skip(1))?;
    if args::write_read_only(
        &args.mode,
        &mut std::io::stdout(),
        workbench_host::areas::browser::renderer::OwnerConfig::installed_executable,
    )? {
        return Ok(());
    }
    let config_dir = args
        .config_dir
        .or_else(config::user_config_dir)
        .ok_or("cannot resolve config directory; pass --config-dir")?;
    let identity = ConfigIdentity::new(&config_dir)?;
    let launch = match args.launch_fd {
        // SAFETY: parsed once at entry, before the Tokio runtime/other threads;
        // inherited descriptor ownership is transferred and validated by adopt.
        Some(fd) => unsafe { LaunchGuard::adopt(&identity, fd)? },
        None => LaunchGuard::acquire(&identity)?,
    };
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(
            identity.config_dir().to_path_buf(),
            args.web,
            launch,
            #[cfg(target_os = "linux")]
            mask,
        ))
}
async fn serve(
    config_dir: PathBuf,
    web: bool,
    launch: LaunchGuard,
    #[cfg(target_os = "linux")] mask: core_instance::handoff::BlockedShutdownSignals,
) -> Result<(), BootstrapError> {
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
    #[cfg(target_os = "linux")]
    let signals = OperatorSignals::new(&mask)?;
    #[cfg(not(target_os = "linux"))]
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
        .run_with_web(&host, backend.clone(), listener, server, signals)
        .await;
    host.shutdown().await?;
    drop(backend);
    drop(host); // storage leases before the lifetime lock
    drop(instance);
    outcome?;
    Ok(())
}
