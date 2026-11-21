//! Test-binary physical entry: real exec, inherited OFD, ticket and owner claim.
use core_instance::{
    ConfigIdentity, InheritedHandoffProof, Instance, LaunchGuard,
    handoff::{
        BlockedShutdownSignals, HandoffRecord,
        boot::BootDeadline,
        entry::{BootMode, EntryOptions},
        running_elf_identity,
    },
};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};
use storage::recovery::{restart::*, *};
pub const ROOT: &str = "EVORCH_V1B2B_PHYSICAL_ROOT";
pub const SOCKETS: &str = "EVORCH_V1B2B_PHYSICAL_SOCKETS";
pub const TX: &str = "composed-physical-tx";
const CAPTURE: &str = "composed-physical-capture";
pub struct Entry {
    pub instance: Arc<Instance>,
    pub proof: InheritedHandoffProof,
    pub mask: Arc<BlockedShutdownSignals>,
}
static ENTRY: OnceLock<Mutex<Option<Entry>>> = OnceLock::new();
pub fn take() -> Option<Entry> {
    ENTRY.get()?.lock().unwrap().take()
}
#[used]
#[unsafe(link_section = ".init_array")]
static PHYSICAL_ENTRY: extern "C" fn() = {
    extern "C" fn entry() {
        let Some(root) = std::env::var_os(ROOT).map(PathBuf::from) else {
            return;
        };
        let sockets = PathBuf::from(std::env::var_os(SOCKETS).unwrap());
        let mask = Arc::new(BlockedShutdownSignals::at_entry().unwrap());
        let args = std::env::args_os().skip(1).collect::<Vec<_>>();
        let options = EntryOptions::parse(args.clone()).unwrap();
        let identity = ConfigIdentity::new(&root).unwrap();
        match options.mode {
            BootMode::PlannedHandoff(planned) => {
                // SAFETY: constructor precedes libtest and owns inherited FDs.
                let inspected = unsafe { planned.inspect() }.unwrap();
                let (instance, proof) =
                    Instance::adopt_handoff(&identity, &sockets, inspected).unwrap();
                assert!(
                    ENTRY
                        .set(Mutex::new(Some(Entry {
                            instance: Arc::new(instance),
                            proof,
                            mask,
                        })))
                        .is_ok()
                );
            }
            BootMode::Cold => {
                let launch = LaunchGuard::acquire(&identity).unwrap();
                let instance = Instance::acquire(&launch, &sockets).unwrap();
                drop(launch);
                seed(&root, &sockets, &identity);
                let record = HandoffRecord {
                    restart_id: TX.into(),
                    config_identity: identity.identity().into(),
                    predecessor_epoch: 1,
                    successor_epoch: 2,
                    nonce_hash: "composed-nonce".into(),
                    successor_build: running_elf_identity().unwrap(),
                    boot_deadline: BootDeadline::start().unwrap(),
                };
                // SAFETY: seed workers/writer are joined; this constructor is
                // single-threaded and owns its entire FD inventory.
                let result = unsafe {
                    instance.exec_handoff(&mask, &std::env::current_exe().unwrap(), &args, record)
                };
                panic!("physical fixture exec failed: {result:?}");
            }
        }
    }
    entry
};
fn seed(root: &Path, sockets: &Path, identity: &ConfigIdentity) {
    let config = storage::StorageConfig {
        db_path: root.join("evorch-events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let writer = storage.handle();
    let bus = Arc::new(event_bus::EventBus::new(64));
    let owner = runtime::ownership::OwnerHost::open_planned(
        &sockets.join("owners"),
        Default::default(),
        bus,
    )
    .unwrap();
    let run = runtime::RunId::new(1);
    let mut permit = owner.start("owned").unwrap();
    permit.run_id = Some(run.to_string());
    permit.begin_turn().unwrap();
    let intent = runtime::quiesce::ResumeIntent {
        version: 1,
        run_id: run,
        incarnation: "old".into(),
        parent_run_id: None,
        role: "Worker".into(),
        wait: runtime::quiesce::ResumeWait::AdmissionQueued,
        initial_input: Some(runtime::quiesce::DurableInput {
            id: "initial".into(),
            text: "physical fixture".into(),
            trusted_user: false,
        }),
        inputs: vec![],
        active: true,
        resume_requested: false,
        stop_reason: None,
        checkpoint: None,
        snapshot: None,
        tool_intent: vec![],
        provider_input: None,
        boundary_pending: false,
        responses_replay: Default::default(),
        compactions: vec![],
        route_pins: vec![],
    };
    writer
        .save_resume_intent(
            storage::resume_intent::ResumeIntentRecord {
                run_id: run.to_string(),
                incarnation: "old".into(),
                payload: serde_json::to_string(&intent).unwrap(),
            },
            None,
        )
        .unwrap();
    let expected = RestartExpectedMember {
        run_id: run.to_string(),
        incarnation: "old".into(),
        root_run_id: run.to_string(),
        thread_id: Some("owned".into()),
        intent_revision: 0,
        snapshot: None,
        owner_id: Some(owner.owner_id().into()),
        owner_generation: Some(permit.lease.generation),
        lane_id: None,
    };
    let build = running_elf_identity().unwrap();
    let schema = u32::try_from(
        storage::Database::open(&config)
            .unwrap()
            .pragma_i64("user_version")
            .unwrap(),
    )
    .unwrap();
    let metadata = RestartMetadata {
        restart_id: TX.into(),
        capture_id: CAPTURE.into(),
        config_identity: identity.identity().into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: build.clone(),
        successor_build: build,
        target_identity: "current".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: schema,
        old_builtin_tool_names: BTreeSet::new(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    let mut tx = writer
        .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
        .unwrap();
    let mut step = |mutation| {
        tx = writer
            .mutate_restart(TX.into(), Some(tx.revision), mutation)
            .unwrap()
    };
    step(RestartMutation::BeginCapture(vec![expected]));
    let record = runtime::quiesce::RunRecoveryRecord {
        version: 1,
        run_id: run,
        old_incarnation: "old".into(),
        intent_revision: 0,
        parent: None,
        root: run,
        thread: Some("owned".into()),
        project: Some(root.into()),
        role: "Worker".into(),
        recipe: runtime::quiesce::RecoveryRecipe {
            settings_reference: runtime::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
            policy: runtime::quiesce::ResumePolicy::CoreOwned,
            goal_id: None,
            team: None,
            lane: None,
            descriptor: None,
        },
        waits: vec![],
        deliveries: vec![],
        next_message_id: 1,
        completion: None,
        completion_generation: None,
        auto_wakes: 0,
        user_reset_generation: 0,
        input_ids: vec![],
    };
    step(RestartMutation::SaveComponent(RecoveryManifestRecord {
        manifest_id: "owned-graph".into(),
        schema_version: 1,
        committed: false,
        superseded: false,
        edges: vec![],
        members: vec![RecoveryMemberRecord {
            run_id: run.to_string(),
            incarnation: "old".into(),
            intent_revision: 0,
            snapshot: None,
            resume: true,
            record_json: serde_json::to_string(&record).unwrap(),
        }],
    }));
    step(RestartMutation::Commit);
    let active = BTreeMap::from([(
        "owned".into(),
        runtime::ownership::ExpectedThreadRuns {
            generation: permit.lease.generation,
            active_runs: BTreeSet::from([run.to_string()]),
        },
    )]);
    let holds = owner.hold_for_restart(TX, CAPTURE, &active).unwrap();
    owner.stop_worker_for_exec().unwrap();
    for hold in holds {
        step(RestartMutation::OwnerProgress(RestartOwnerProgress {
            thread_id: hold.thread_id,
            old_owner_id: hold.owner_id,
            old_generation: hold.generation,
            active_runs: hold.active_runs,
            parked_runs: hold.parked_runs,
            hold_saved: true,
            hold_read_back: true,
            worker_joined: true,
            socket_unlinked: true,
            hold_removed: false,
        }));
    }
    step(RestartMutation::Arm {
        nonce_hash: "composed-nonce".into(),
    });
    drop(permit);
    drop(owner);
    drop(writer);
    storage.close();
}
pub struct ParkedRecovery;
struct ParkedDiagnostics;
impl workbench_host::bootstrap::core_startup::CoreStartupDiagnostics for ParkedDiagnostics {
    fn snapshot(&self) -> serde_json::Value {
        serde_json::json!({})
    }
    fn has_pending_work(&self) -> bool {
        false
    }
}
impl workbench_host::bootstrap::core_startup::CoreStartupHook for ParkedRecovery {
    fn start<'a>(
        &'a self,
        _: workbench_host::bootstrap::core_startup::CoreStartupInputs,
        _: &'a mut workbench_host::bootstrap::services::BackgroundServices,
    ) -> workbench_host::bootstrap::core_startup::CoreStartupFuture<'a> {
        Box::pin(async {
            Ok(Arc::new(ParkedDiagnostics)
                as Arc<
                    dyn workbench_host::bootstrap::core_startup::CoreStartupDiagnostics,
                >)
        })
    }
}
mod lease_observation;
pub use lease_observation::{State, finish_renewals, wait_for_renewal};

pub fn state(
    host: &workbench_host::bootstrap::BootstrappedHost,
    driver: &crate::lifecycle::LifecycleDriver,
) -> State {
    State::new(
        format!(
            "{:?}|{:?}|{:?}|{:?}",
            storage::Database::open(host.storage_config())
                .unwrap()
                .restart_transactions()
                .unwrap(),
            driver.restart.snapshot(),
            host.runtime.quiesce_state(),
            host.host
                .lock()
                .unwrap()
                .service()
                .lane_bindings()
                .observe()
                .is_current()
        ),
        host.owner.restart_inventory().unwrap(),
    )
}
