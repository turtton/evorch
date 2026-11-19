use super::*;
use crate::{signal_entry, without_artifact_tool::WithoutArtifactTool};
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use evorch_core::{
    backend::HostBackend,
    lifecycle::{LifecycleDriver, OperatorSignals},
    restart::{FinalizationEnvironment, FinalizationTarget},
};
use std::{
    collections::BTreeMap,
    sync::{Arc, atomic::AtomicBool},
};
use storage::recovery::restart::{RestartMetadata, RestartMutation};
use workbench_host::bootstrap::{
    BootstrapOptions, BootstrappedHost, TestSeams,
    models::{ProductionModel, compose_production_model},
};
struct Direct;
impl runtime::SandboxFactory for Direct {
    fn build(
        &self,
        _: &runtime::ExecutionPolicy,
        _: &runtime::WorkspaceMounts,
    ) -> Result<Arc<dyn sandbox::Sandbox>, sandbox::SandboxError> {
        Ok(Arc::new(sandbox::DirectSandbox::new_unchecked()))
    }
}
async fn wait(events: &mut event_bus::EventReceiver, run: runtime::RunId) {
    loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id,
            to,
            reason,
            ..
        }) = events.recv().await.unwrap().kind
            && run_id == run.to_string()
        {
            eprintln!("cache predecessor phase: {to:?} ({reason:?})");
            if to == event_bus::AgentRunPhase::Waiting {
                return;
            }
            assert_ne!(to, event_bus::AgentRunPhase::Error, "{reason:?}");
        }
    }
}
pub(crate) fn run() {
    let root = PathBuf::from(std::env::var_os("EVORCH_EXEC_CACHE_ROOT").unwrap());
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(run_async(root));
}
async fn run_async(root: PathBuf) {
    let config_dir = root.join("config");
    let options = config::LoadOptions {
        user_config_dir: Some(config_dir.clone()),
        project_dir: Some(root.clone()),
        read_env: false,
        ..Default::default()
    };
    let current = config::Config::load_strict(&options).unwrap();
    let bus = Arc::new(event_bus::EventBus::new(1024));
    let mut provider_events = bus.subscribe();
    let model = compose_production_model(
        &current,
        &ProductionModel {
            load_options: options,
            credential_store: Arc::new(
                sandbox::credential::FileCredentialStore::open(root.join("credentials")).unwrap(),
            ),
            bus,
            env: Arc::new(routing::MapEnv::from_iter([(
                "EVORCH_EXEC_CACHE_KEY",
                "offline-exec-cache-key",
            )])),
            model_catalog_dir: Some(root.join("cache/evorch")),
        },
    )
    .unwrap();
    let old_catalog = std::env::var("EVORCH_EXEC_CACHE_OLD_CATALOG").unwrap() == "1";
    let model = Arc::new(WithoutArtifactTool(
        model,
        Arc::new(AtomicBool::new(old_catalog)),
    ));
    let bootstrap = BootstrapOptions {
        config_dir: config_dir.clone(),
        project_root: root.clone(),
        ownership_root: PathBuf::from(std::env::var_os("XDG_STATE_HOME").unwrap())
            .join("evorch/threads"),
    };
    let seams = TestSeams {
        model,
        sandbox: Arc::new(Direct),
    };
    let disconnected_mcp = std::env::var("EVORCH_EXEC_CACHE_MCP").unwrap() == "1";
    let (mut host, mcp_prepared) = if disconnected_mcp {
        let (mcp, prepared) = super::mcp::services(&config_dir);
        // Only this cold, empty predecessor uses embedded bootstrap to inject
        // MCP. It has no graph to recover, so omitting its Core startup scan
        // proves nothing about cold-core recovery. LifecycleDriver below still
        // executes the production M2 ToolSnapshot hook before Commit, raw exec,
        // and the real successor's CoreGraphRecovery/Ready activation.
        (
            BootstrappedHost::open_with_mcp_test_seams(bootstrap, seams, mcp)
                .await
                .unwrap(),
            Some(prepared),
        )
    } else {
        (
            BootstrappedHost::open_for_core_with_test_seams(
                bootstrap,
                seams,
                &evorch_core::recovery_service::CoreGraphRecovery,
            )
            .await
            .unwrap(),
            None,
        )
    };
    if old_catalog {
        // The fixture ELF represents a different compiled builtin catalog. Both
        // the live model and the M2 factory must agree on the missing tool.
        let mut catalog = host.runtime.restart_builtin_tool_catalog(&[]);
        catalog.retain(|tool| tool.name != "inspect_artifact");
        host.runtime
            .set_restart_builtin_catalog_for_fixture(catalog)
            .unwrap();
    }
    let identity = ConfigIdentity::new(&config_dir).unwrap();
    let launch = LaunchGuard::acquire(&identity).unwrap();
    let instance = Arc::new(Instance::acquire_default(&launch).unwrap());
    drop(launch);
    let mut driver = LifecycleDriver::new(&host);
    let core = driver.restart.clone();
    let backend = Arc::new(HostBackend::new(
        host.host.clone(),
        instance.identity.clone(),
    ));
    let listener = instance.bind().unwrap();
    listener.set_nonblocking(true).unwrap();
    let listener = tokio::net::UnixListener::from_std(listener).unwrap();
    let signals = OperatorSignals::new(signal_entry::mask()).unwrap();
    let permit = host.owner.start("cache-thread").unwrap();
    let mut events = host.runtime.event_bus().subscribe();
    let run = host
        .runtime
        .delegate_chat(
            "cache-thread",
            runtime::Role::Orchestrator,
            "Preserve this exact prefix and the tool result".into(),
            runtime::RunConfig {
                ownership: Some(permit),
                project_root: Some(root.clone()),
                memory: workbench_host::lane::memory_boundary(
                    &host.runtime,
                    Some(&host.composition.storage),
                    Some(&root),
                ),
                interactive: true,
                conversation: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    wait(&mut events, run).await;
    let mut usage = Vec::new();
    while usage.len() < 2 {
        if let event_bus::EventKind::Provider(event_bus::ProviderEvent::RequestCompleted {
            input_tokens,
            cache_read_tokens,
            ..
        }) = provider_events.recv().await.unwrap().kind
        {
            usage.push((input_tokens, cache_read_tokens));
        }
    }
    host.composition
        .storage_writer
        .upsert_thread_runtime(&storage::threads::ThreadUpsert {
            thread_id: "cache-thread".into(),
            project_root: Some(root.to_string_lossy().into_owned()),
            root_run_id: Some(run.to_string()),
            chat_role: Some(runtime::Role::Orchestrator.name().into()),
            ..Default::default()
        })
        .unwrap();
    let id = core
        .request_planned(BTreeMap::from([(
            run,
            runtime::quiesce::RecoveryRecipe {
                settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
                policy: runtime::quiesce::ResumePolicy::CoreOwned,
                goal_id: None,
                team: None,
                lane: None,
                descriptor: None,
            },
        )]))
        .unwrap();
    core.wait_sealed(id).await.unwrap();
    let old_input = host
        .runtime
        .resume_intents()
        .unwrap()
        .into_iter()
        .find(|intent| intent.run_id == run)
        .unwrap()
        .provider_input
        .unwrap();
    assert_eq!(old_input.affinity_key, run.to_string());
    let stored_affinity: String = rusqlite::Connection::open(&host.composition.storage.db_path)
        .unwrap()
        .query_row(
            "SELECT json_extract(payload,'$.provider_input.affinity_key') FROM resume_intents WHERE run_id=?1",
            [run.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(stored_affinity, run.to_string());
    let product = PathBuf::from(std::env::var_os("EVORCH_EXEC_CACHE_PRODUCT").unwrap());
    // Probe the actual product, never a wrapper's fabricated description.
    let current = Arc::new(core_launcher::handoff::CurrentElf::record().unwrap());
    let storage_version = rusqlite::Connection::open(&host.composition.storage.db_path)
        .unwrap()
        .pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0))
        .unwrap();
    let platform = format!("linux-{}", std::env::consts::ARCH);
    let checked = core_launcher::handoff::targets::RestartTarget::preflight(
        core_launcher::handoff::targets::TargetSelection {
            id: "cache-target",
            configured_path: Some(&product),
            timeout: std::time::Duration::from_secs(60),
        },
        &current,
        core_launcher::handoff::targets::TargetRequirements {
            handoff: 1,
            snapshot: 1,
            storage_schema: storage_version,
            protocol: workbench_protocol::ProtocolVersion::CURRENT.0,
            platform: &platform,
        },
        core_launcher::handoff::ProbeLimits::default(),
        std::future::pending(),
    )
    .await
    .unwrap()
    .before_capture(
        &current,
        core_launcher::handoff::ProbeLimits::default(),
        std::future::pending(),
    )
    .await
    .unwrap();
    let target =
        FinalizationTarget::new(current, checked).with_environment(FinalizationEnvironment::new(
            instance.clone(),
            signal_entry::mask().clone(),
            signals.clone(),
            driver.control_drain(),
            vec!["--config-dir".into(), config_dir.into_os_string()],
        ));
    if let Some(prepared) = mcp_prepared {
        assert_eq!(prepared.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
    let metadata = RestartMetadata {
        restart_id: "exec-cache".into(),
        capture_id: "exec-cache-capture".into(),
        config_identity: instance.identity.clone(),
        old_epoch: backend.hello().instance_epoch.0,
        successor_epoch: backend.hello().instance_epoch.0.checked_add(1).unwrap(),
        old_build: core_instance::handoff::running_elf_identity().unwrap(),
        successor_build: core_instance::handoff::elf_identity(&product).unwrap(),
        target_identity: "cache-target".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version,
        old_builtin_tool_names: host.runtime.restart_builtin_tool_names(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    let tx = host
        .composition
        .storage_writer
        .mutate_restart(
            "exec-cache".into(),
            None,
            RestartMutation::Create(Box::new(metadata)),
        )
        .unwrap();
    let usage_watermark: i64 = rusqlite::Connection::open(&host.composition.storage.db_path)
        .unwrap()
        .query_row(
            "SELECT COALESCE(MAX(rowid), 0) FROM usage_requests",
            [],
            |r| r.get(0),
        )
        .unwrap();
    println!(
        "CACHE-BEFORE {}",
        json!({"pid":std::process::id(), "run":run, "usage":usage, "usage_watermark":usage_watermark})
    );
    std::io::stdout().flush().unwrap();
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).unwrap();
        assert_eq!(line, "exec\n");
    })
    .await
    .unwrap();
    core.start_finalization(id, &mut host, tx, target).unwrap();
    tokio::select! {
        result=driver.run(&host, backend, listener, false, signals) => panic!("lifecycle returned before exec: {result:?}"),
        result=core.wait_finalization(id) => panic!("M2 returned before exec: {result:?}"),
    }
}
