//! Real exec + sealed-ticket + inherited-OFD bootstrap shared by I-6 tests.
//! No inherited-proof/evidence/authority constructor is used here.
use core_instance::handoff::{
    BlockedShutdownSignals, HandoffRecord,
    boot::BootDeadline,
    entry::{BootMode, EntryOptions},
    running_elf_identity,
};
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use event_bus::EventBus;
use runtime::{AgentRuntime, OwnedThreadCas, RunStore, ownership::*, quiesce::*};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
    sync::Arc,
};
use storage::{
    Database, Storage, StorageConfig,
    recovery::{graph_control::GraphControlCas, restart::*, *},
};

pub const TX: &str = "i6-restart";
pub const CAPTURE: &str = "i6-capture";
pub const THREADS: [&str; 2] = ["a", "b"];
pub fn run_id(thread: &str) -> runtime::RunId {
    runtime::RunId::new(match thread {
        "a" => 1,
        "b" => 2,
        other => 3 + other.strip_prefix('t').unwrap().parse::<u64>().unwrap(),
    })
}

struct NeverModel;
#[async_trait::async_trait]
impl runtime::AgentModel for NeverModel {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "never-called".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        panic!("I-6 must not make a provider request")
    }
}
pub struct Fixture {
    pub root: PathBuf,
    pub runtime: AgentRuntime,
    pub owner: Arc<OwnerHost>,
    pub storage: Storage,
    pub config: StorageConfig,
    _instance: Instance,
}
impl Fixture {
    pub fn db(&self) -> Database {
        Database::open(&self.config).unwrap()
    }
    pub fn tx(&self) -> RestartTransaction {
        self.db().restart_transaction(TX).unwrap().unwrap()
    }
    pub fn cas(&self, thread: &str) -> GraphControlCas {
        self.db()
            .recovery_graph_cas(TX, &format!("graph-{thread}"))
            .unwrap()
    }
    pub fn owners(&self, thread: &str) -> Vec<OwnedThreadCas> {
        let owner = self.owner.attach(thread).unwrap();
        vec![OwnedThreadCas {
            thread_id: thread.into(),
            owner_identity: Some(owner.lease.owner_id),
            owner_generation: Some(owner.lease.generation),
        }]
    }
    pub fn denied(&self, thread: &str) {
        assert!(
            self.runtime
                .disposal_authority(self.cas(thread), self.owners(thread))
                .is_err(),
            "{thread}: ordinary ownership must not become retained physical authority"
        );
    }
    pub fn allowed(&self, thread: &str) {
        self.runtime
            .disposal_authority(self.cas(thread), self.owners(thread))
            .unwrap_or_else(|e| {
                panic!("{thread}: validated sealed bootstrap must grant disposal: {e}")
            });
    }
    pub fn retain(&self) {
        let report = self
            .runtime
            .claim_physical_bootstrap_owners(
                &self.owner,
                self.runtime.adopted_restart_ownership_evidence().unwrap(),
                || {},
            )
            .unwrap();
        assert!(report.blocked_threads.is_empty());
    }
    pub fn registry(&self) -> Registry {
        Registry::open_existing(&self.root.join("owners/owners.db")).unwrap()
    }
    pub fn hold(&self, thread: &str) -> PlannedHold {
        self.owner
            .attach(thread)
            .unwrap()
            .handoff_hold
            .unwrap()
            .as_ref()
            .clone()
    }
    pub fn step(&self, mutation: RestartMutation) {
        let tx = self.tx();
        self.storage
            .handle()
            .mutate_restart(TX.into(), Some(tx.revision), mutation)
            .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.owner.stop_worker_for_exec().unwrap();
    }
}
pub fn recovery_record(root: &Path, thread: &str) -> RunRecoveryRecord {
    let run = run_id(thread);
    RunRecoveryRecord {
        version: 1,
        run_id: run,
        old_incarnation: "old".into(),
        intent_revision: 0,
        parent: None,
        root: run,
        thread: Some(thread.into()),
        project: Some(root.join("project")),
        role: "Worker".into(),
        recipe: RecoveryRecipe {
            settings_reference: CORE_RECOVERY_SETTINGS_REFERENCE.into(),
            policy: ResumePolicy::CoreOwned,
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
    }
}
fn storage_config(root: &Path) -> StorageConfig {
    StorageConfig {
        db_path: root.join("main.db"),
        ..Default::default()
    }
}
fn seed(root: &Path, identity: &ConfigIdentity, case: &str) {
    std::fs::create_dir_all(root.join("project")).unwrap();
    let storage = Storage::open(storage_config(root)).unwrap();
    let handle = storage.handle();
    let bus = Arc::new(EventBus::new(64));
    let owner = OwnerHost::open_planned(&root.join("owners"), Default::default(), bus).unwrap();
    let mut permits = BTreeMap::new();
    let mut expected = vec![];
    let threads: Vec<String> = if case == "diagnostic-oversize" {
        THREADS
            .into_iter()
            .map(str::to_owned)
            .chain((0..63).map(|n| format!("t{n:03}")))
            .collect()
    } else {
        THREADS.into_iter().map(str::to_owned).collect()
    };
    for thread in threads.iter().map(String::as_str) {
        let mut permit = owner.start(thread).unwrap();
        permit.run_id = Some(run_id(thread).to_string());
        permit.begin_turn().unwrap();
        let run = run_id(thread).to_string();
        let intent = ResumeIntent {
            version: 1,
            run_id: run_id(thread),
            incarnation: "old".into(),
            parent_run_id: None,
            role: "Worker".into(),
            wait: ResumeWait::AdmissionQueued,
            initial_input: Some(DurableInput {
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
        handle
            .save_resume_intent(
                storage::resume_intent::ResumeIntentRecord {
                    run_id: run.clone(),
                    incarnation: "old".into(),
                    payload: serde_json::to_string(&intent).unwrap(),
                },
                None,
            )
            .unwrap();
        expected.push(RestartExpectedMember {
            run_id: run.clone(),
            incarnation: "old".into(),
            root_run_id: run,
            thread_id: Some(thread.into()),
            intent_revision: 0,
            snapshot: None,
            owner_id: Some(owner.owner_id().into()),
            owner_generation: Some(permit.lease.generation),
            lane_id: None,
        });
        permits.insert(thread, permit);
    }
    let build = running_elf_identity().unwrap();
    let metadata = RestartMetadata {
        restart_id: TX.into(),
        capture_id: CAPTURE.into(),
        config_identity: identity.identity().into(),
        old_epoch: 1,
        successor_epoch: 2,
        old_build: build.clone(),
        successor_build: build,
        target_identity: "self".into(),
        handoff_version: 1,
        snapshot_version: 1,
        storage_version: 71,
        old_builtin_tool_names: BTreeSet::new(),
        old_compiled_catalog_fingerprint: None,
        new_compiled_catalog_fingerprint: None,
        old_effective_tools_fingerprint: None,
        new_effective_tools_fingerprint: None,
    };
    let mut tx = handle
        .mutate_restart(TX.into(), None, RestartMutation::Create(Box::new(metadata)))
        .unwrap();
    let mut step = |m| {
        tx = handle
            .mutate_restart(TX.into(), Some(tx.revision), m)
            .unwrap();
    };
    step(RestartMutation::BeginCapture(expected.clone()));
    let groups: Vec<_> = if matches!(case, "i9-batch" | "diagnostic-oversize") {
        vec![expected.iter().collect::<Vec<_>>()]
    } else {
        expected.iter().map(|m| vec![m]).collect()
    };
    for members in groups {
        let thread = members[0].thread_id.as_deref().unwrap();
        step(RestartMutation::SaveComponent(RecoveryManifestRecord {
            manifest_id: format!("graph-{thread}"),
            schema_version: 1,
            committed: false,
            superseded: false,
            edges: vec![],
            members: members
                .into_iter()
                .map(|m| RecoveryMemberRecord {
                    run_id: m.run_id.clone(),
                    incarnation: m.incarnation.clone(),
                    intent_revision: 0,
                    snapshot: None,
                    resume: true,
                    record_json: serde_json::to_string(&recovery_record(
                        root,
                        m.thread_id.as_deref().unwrap(),
                    ))
                    .unwrap(),
                })
                .collect(),
        }));
    }
    step(RestartMutation::Commit);
    let active = permits
        .iter()
        .map(|(thread, p)| {
            (
                thread.to_string(),
                ExpectedThreadRuns {
                    generation: p.lease.generation,
                    active_runs: BTreeSet::from([p.run_id.clone().unwrap()]),
                },
            )
        })
        .collect();
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
        nonce_hash: "i6-nonce".into(),
    });
    drop(permits);
    drop(owner);
    drop(handle);
    storage.close();
}
#[path = "physical_test_harness.rs"]
mod harness;

/// Each case is a fresh process: physical adoption must precede all DB/owner/runtime threads.
/// Child exit is the completion event; no sleeps, polling, or test-local timeout.
pub type RuntimeFactory =
    fn(&str, Arc<EventBus>) -> (Arc<::tools::ToolExecutor>, Arc<dyn runtime::AgentModel>);

pub fn default_factory(
    _: &str,
    bus: Arc<EventBus>,
) -> (Arc<::tools::ToolExecutor>, Arc<dyn runtime::AgentModel>) {
    (
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(NeverModel),
    )
}

pub fn run(cases: &[&str], check: fn(&Fixture, &str)) {
    run_with(cases, check, default_factory);
}

/// Test-only dependency composition, after real sealed adoption. No authority
/// constructor or access to runtime internals is introduced.
pub fn run_with(cases: &[&str], check: fn(&Fixture, &str), factory: RuntimeFactory) {
    let args = std::env::args_os().skip(1).collect::<Vec<_>>();
    let value = |flag: &str| args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    let Some(root) = value("--fixture-root").map(PathBuf::from) else {
        let Some(selected) = harness::select(cases, &args) else {
            return;
        };
        for case in selected {
            let dir = test_socket_dir::SocketDir::new();
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--case", case, "--fixture-root"])
                .arg(dir.path())
                .status()
                .unwrap();
            assert!(status.success(), "I-6 {case} failed: {status}");
            println!("I-6 {case}: PASS");
        }
        return;
    };
    let case = value("--case").unwrap().into_string().unwrap();
    assert!(
        cases.contains(&case.as_str()),
        "unknown physical fixture case {case}"
    );
    let signals = BlockedShutdownSignals::at_entry().unwrap();
    let options = EntryOptions::parse(args.clone()).unwrap();
    let entry = match options.mode {
        BootMode::Cold => None,
        BootMode::PlannedHandoff(p) => Some(unsafe { p.inspect() }.unwrap()),
    };
    let identity = ConfigIdentity::new(&root.join("config")).unwrap();
    let (instance, proof) = if let Some(entry) = entry {
        Instance::adopt_handoff(&identity, &root, entry).unwrap()
    } else {
        let launch = LaunchGuard::acquire(&identity).unwrap();
        let instance = Instance::acquire(&launch, &root).unwrap();
        drop(launch);
        seed(&root, &identity, &case);
        let record = HandoffRecord {
            restart_id: TX.into(),
            config_identity: identity.identity().into(),
            predecessor_epoch: 1,
            successor_epoch: 2,
            nonce_hash: "i6-nonce".into(),
            successor_build: running_elf_identity().unwrap(),
            boot_deadline: BootDeadline::start().unwrap(),
        };
        // SAFETY: all fixture-owned workers are stopped/joined, no children exist;
        // this single-threaded entry owns the entire FD inventory and signal mask.
        let result = unsafe {
            instance.exec_handoff(&signals, &std::env::current_exe().unwrap(), &args, record)
        };
        panic!("physical exec failed: {result:?}");
    };
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        let config = storage_config(&root);
        let storage = Storage::open(config.clone()).unwrap();
        let bus = Arc::new(EventBus::new(64));
        let owner = Arc::new(
            OwnerHost::open_planned(&root.join("owners"), Default::default(), bus.clone()).unwrap(),
        );
        let (tools, model) = factory(&case, bus.clone());
        let runtime = AgentRuntime::new(bus, tools, model)
            .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        runtime
            .adopt_restart_handoff(proof)
            .unwrap_or_else(|e| panic!("DB Adopt failed: {}", e.error));
        let f = Fixture {
            root,
            runtime,
            owner,
            storage,
            config,
            _instance: instance,
        };
        assert_eq!(f.tx().phase, RestartPhase::Adopted);
        check(&f, &case);
    });
}
