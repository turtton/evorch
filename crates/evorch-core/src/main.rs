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
#[cfg(target_os = "linux")]
mod planned_entry;
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
    let mask = Arc::new(core_instance::handoff::BlockedShutdownSignals::at_entry()?);
    #[cfg(target_os = "linux")]
    let entry = core_instance::handoff::entry::EntryOptions::parse(std::env::args_os().skip(1))?;
    // Inspect before config/DB/runtime/any new ShutdownInbox. Never fall back to Cold.
    #[cfg(target_os = "linux")]
    let inherited = match entry.mode {
        core_instance::handoff::entry::BootMode::Cold => None,
        core_instance::handoff::entry::BootMode::PlannedHandoff(handoff) => {
            // SAFETY: exclusive OS FDs at single-threaded process entry.
            Some(unsafe { handoff.inspect()? })
        }
    };
    #[cfg(target_os = "linux")]
    let args = args::parse(
        entry
            .args
            .into_iter()
            .map(|arg| arg.into_string().map_err(|_| "invalid UTF-8 option"))
            .collect::<Result<Vec<_>, _>>()?,
    )?;
    #[cfg(not(target_os = "linux"))]
    let args = args::parse(std::env::args().skip(1))?;
    #[cfg(target_os = "linux")]
    if let Some(inherited) = inherited {
        if args.mode != args::Mode::Serve || args.launch_fd.is_some() {
            return Err("invalid planned handoff mode".into());
        }
        return planned_entry::run(args, mask, inherited);
    }
    if args.mode == args::Mode::Help {
        args::write_read_only(&args.mode, &mut std::io::stdout(), || unreachable!())?;
        return Ok(());
    }
    sandbox::owned::bind_installed("core").map_err(|_| "process guard bundle validation failed")?;
    #[cfg(target_os = "linux")]
    if entry.describe {
        serde_json::to_writer(
            std::io::stdout(),
            &evorch_core::handoff_description::describe()?,
        )?;
        println!();
        return Ok(());
    }
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
    #[cfg(target_os = "linux")]
    {
        let inbox = mask.inbox()?;
        evorch_core::lifecycle::shutdown_watchdog::run_with_runtime(
            (identity.config_dir().to_path_buf(), args.web, launch, mask),
            inbox,
            |(config_dir, web, launch, mask), runtime, inbox, observer| {
                runtime.block_on(serve(config_dir, web, launch, inbox, observer, mask))
            },
        )
        .map_err(|failure| -> BootstrapError { failure.error.into() })?
    }
    #[cfg(not(target_os = "linux"))]
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(serve(identity.config_dir().to_path_buf(), args.web, launch))
}
async fn serve(
    config_dir: PathBuf,
    web: bool,
    launch: LaunchGuard,
    #[cfg(target_os = "linux")] inbox: core_instance::handoff::ShutdownInbox,
    #[cfg(target_os = "linux")] observer: evorch_core::lifecycle::shutdown_watchdog::Observer,
    #[cfg(target_os = "linux")] mask: Arc<core_instance::handoff::BlockedShutdownSignals>,
) -> Result<(), BootstrapError> {
    let instance = Arc::new(Instance::acquire_default(&launch)?);
    #[cfg(target_os = "linux")]
    let options =
        evorch_core::lifecycle::product_exit::ProductOptions::new(config_dir.clone(), web);
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
    let signals = OperatorSignals::from_inbox(inbox)?;
    #[cfg(not(target_os = "linux"))]
    let signals = OperatorSignals::new()?;
    let mut lifecycle = LifecycleDriver::new(&host);
    #[cfg(target_os = "linux")]
    lifecycle.set_shutdown_watchdog(observer)?;
    #[cfg(target_os = "linux")]
    let exit = evorch_core::lifecycle::product_exit::ProductExitContext::new(
        instance.clone(),
        mask,
        signals.clone(),
        lifecycle.control_drain(),
        options,
    );
    let inputs = evorch_core::admin_composition::Inputs::new(&host, &backend);
    #[cfg(target_os = "linux")]
    let inputs = inputs.with_finalization(lifecycle.finalization(), &exit);
    evorch_core::admin_composition::install(&inputs);
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
        .run_owned_with_web(
            &mut host,
            backend.clone(),
            listener,
            server,
            signals,
            #[cfg(target_os = "linux")]
            &exit,
        )
        .await;
    host.shutdown().await?;
    drop(backend);
    drop(host); // storage leases before the lifetime lock
    #[cfg(target_os = "linux")]
    drop(exit);
    drop(instance);
    outcome?;
    Ok(())
}
