//! Internal planned successor dispatch; no restart operation/capability publication.
use super::*;
use core_instance::handoff::{BlockedShutdownSignals, HandoffEntry, boot::BootFailure};
use evorch_core::{lifecycle::OperatorSignals, planned_bootstrap::PlannedBoot};

pub fn run(
    args: args::Args,
    mask: Arc<BlockedShutdownSignals>,
    inherited: HandoffEntry,
) -> Result<(), BootstrapError> {
    let config_dir = args
        .config_dir
        .or_else(config::user_config_dir)
        .ok_or("cannot resolve config directory; pass --config-dir")?;
    let identity = ConfigIdentity::new(&config_dir)?;
    let core_identity = identity.identity().to_owned();
    let config_dir = identity.config_dir().to_path_buf();
    let options =
        evorch_core::lifecycle::product_exit::ProductOptions::new(config_dir.clone(), args.web);
    let state_dir = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::home_dir().map(|p| p.join(".local/state")))
        .ok_or("cannot resolve ownership state directory")?
        .join("evorch");
    let project_root = std::env::current_dir()?;
    let (mut boot, proof) = PlannedBoot::adopt_default_entry(identity, inherited)?;
    // Physical adoption must precede opening the guard image too: never
    // reuse a descriptor still owned by the handoff evidence.
    if sandbox::owned::bind_installed("core").is_err() {
        boot.fail(BootFailure::AdoptedBeforeReady, None);
    }
    // Entry inspection AND physical adopt precede the first new signal FD.
    let inbox = match mask.inbox() {
        Ok(inbox) => inbox,
        Err(_) => boot.fail(BootFailure::BeforeAdopt, None),
    };
    let result = evorch_core::lifecycle::shutdown_watchdog::run_with_runtime(
        (boot, proof),
        inbox,
        |(boot, proof), runtime, inbox, observer| {
            runtime.block_on(async move {
        let mut pending = boot
            .bootstrap(
                BootstrapOptions {
                    config_dir,
                    ownership_root: state_dir.join("threads"),
                    project_root,
                },
                proof,
                &inbox,
            )
            .await;
        let backend = Arc::new(HostBackend::new(pending.control_host(), core_identity));
        let web_config = pending.web_config(args.web);
        backend.set_web_enabled(web_config.enabled);
        let server = match WebServer::start(web_config, backend.clone()).await {
            Ok(server) => server,
            Err(_) => pending.fail_boot(BootFailure::AdoptedBeforeReady),
        };
        let listener = match pending.bind_control() {
            Ok(listener) => listener,
            Err(_) => pending.fail_boot(BootFailure::AdoptedBeforeReady),
        };
        let at_ms = match std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()
            .and_then(|d| d.as_millis().try_into().ok())
        {
            Some(at_ms) => at_ms,
            None => pending.fail_boot(BootFailure::AdoptedBeforeReady),
        };
        let (mut host, instance, listener, _) =
            pending.confirm_ready(listener, at_ms, &inbox).await;
        // The watchdog no longer reads the inbox. Transfer, do not create a second one.
        let signals = OperatorSignals::from_inbox(inbox)?;
        listener.set_nonblocking(true)?;
        let listener = tokio::net::UnixListener::from_std(listener)?;
        host.activate_after_ready(&CoreGraphRecovery).await?;
        println!(
            "{}",
            serde_json::json!({"status":"ready", "socket":instance.socket, "core":instance.identity,
            "web":server.as_ref().map(|s|s.local_addr().to_string())})
        );
        let mut lifecycle = LifecycleDriver::new(&host);
        lifecycle.set_shutdown_watchdog(observer)?;
        let exit = evorch_core::lifecycle::product_exit::ProductExitContext::new(
            instance.clone(), mask, signals.clone(), lifecycle.control_drain(), options,
        );
        evorch_core::admin_composition::install(
            &evorch_core::admin_composition::Inputs::new(&host, &backend)
                .with_finalization(lifecycle.finalization(), &exit),
        );
        let outcome = lifecycle
            .run_owned_with_web(&mut host, backend.clone(), listener, server, signals, &exit)
            .await;
        host.shutdown().await?;
        drop(backend);
        drop(host);
        drop(exit);
        drop(instance);
        outcome?;
        Ok(())
    })
        },
    );
    match result {
        Ok(result) => result,
        Err(failure) => {
            let (mut boot, _proof) = failure.context;
            boot.fail(BootFailure::BeforeAdopt, None)
        }
    }
}
