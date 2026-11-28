//! Real exec + sealed-ticket + inherited-OFD bootstrap shared by the recovery-graph tests.
//! No inherited-proof/evidence/authority constructor is used here, and no process
//! ownership exists: the graph is adopted through the planned-handoff proof, and
//! disposal runs under the current writer leases and the thread binding.
use core_instance::handoff::{
    BlockedShutdownSignals, HandoffRecord,
    boot::BootDeadline,
    entry::{BootMode, EntryOptions},
    running_elf_identity,
};
use core_instance::{ConfigIdentity, Instance, LaunchGuard};
use event_bus::EventBus;
use runtime::{AgentRuntime, RunStore, quiesce::*, scratch::ThreadScratch};
use std::{
    collections::BTreeSet,
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
    pub storage: Storage,
    pub config: StorageConfig,
    /// The scratch the runtime and its disposal share (one `Arc`).
    pub scratch: Arc<ThreadScratch>,
    /// The existing writer leases the disposal context retains.
    pub leases: Arc<storage::WriterLeases>,
    _maintenance: storage::SharedLeases,
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
    /// Whether disposal authority is granted for this graph right now.
    pub fn authority(&self, thread: &str) -> bool {
        self.runtime.disposal_authority(self.cas(thread)).is_ok()
    }
    pub fn step(&self, mutation: RestartMutation) {
        let tx = self.tx();
        self.storage
            .handle()
            .mutate_restart(TX.into(), Some(tx.revision), mutation)
            .unwrap();
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
            descriptor: Some(
                serde_json::from_value(serde_json::json!({
                    "role": "worker", "interactive": true, "keep_alive": true,
                    "load_skills": [], "restorable": true
                }))
                .unwrap(),
            ),
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
/// The shared state root: the one the writer leases and the thread binding name.
fn state_root(root: &Path) -> PathBuf {
    root.join("state")
}
fn sidebar_path(root: &Path) -> PathBuf {
    root.join("sidebar.json")
}
fn seed(root: &Path, identity: &ConfigIdentity, case: &str) {
    std::fs::create_dir_all(root.join("project")).unwrap();
    std::fs::create_dir_all(state_root(root)).unwrap();
    std::fs::write(sidebar_path(root), "{}").unwrap();
    let storage = Storage::open(storage_config(root)).unwrap();
    let handle = storage.handle();
    let mut expected = vec![];
    let threads: Vec<String> = if matches!(case, "diagnostic-64" | "diagnostic-oversize") {
        let extra = if case == "diagnostic-64" { 62 } else { 63 };
        THREADS
            .into_iter()
            .map(str::to_owned)
            .chain((0..extra).map(|n| format!("t{n:03}")))
            .collect()
    } else {
        THREADS.into_iter().map(str::to_owned).collect()
    };
    for thread in threads.iter().map(String::as_str) {
        let run = run_id(thread).to_string();
        // The thread's current binding: the proof that disposal may touch its scratch.
        handle
            .upsert_thread_runtime(&storage::threads::ThreadUpsert {
                thread_id: thread.into(),
                root_run_id: Some(run.clone()),
                ..Default::default()
            })
            .unwrap();
        rusqlite::Connection::open(&storage_config(root).db_path)
            .unwrap()
            .execute(
                "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,?2,?1,0)",
                rusqlite::params![run, thread],
            )
            .unwrap();
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
                model_preference_update: runtime::ModelPreferenceUpdate::Keep,
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
            lane_id: None,
        });
    }
    let build = running_elf_identity().unwrap();
    // The schema the DB really has, as the production finalization records it.
    let schema = u32::try_from(
        Database::open(&storage_config(root))
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
        target_identity: "self".into(),
        handoff_version: core_instance::handoff::HANDOFF_FORMAT_VERSION,
        snapshot_version: 1,
        storage_version: schema,
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
    let groups: Vec<_> = if matches!(case, "i9-batch" | "diagnostic-64" | "diagnostic-oversize") {
        vec![expected.iter().collect::<Vec<_>>()]
    } else {
        expected.iter().map(|m| vec![m]).collect()
    };
    for members in groups {
        let thread = members[0].thread_id.as_deref().unwrap();
        step(RestartMutation::SaveComponent(RecoveryManifestRecord {
            manifest_id: format!("graph-{thread}"),
            schema_version: RECOVERY_SCHEMA_VERSION,
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
    step(RestartMutation::Arm {
        nonce_hash: "i6-nonce".into(),
    });
    drop(handle);
    storage.close();
}
#[path = "physical_test_harness.rs"]
mod harness;

/// Each case is a fresh process: physical adoption must precede all DB/runtime threads.
/// Child exit is the completion event; no sleeps, polling, or test-local timeout.
pub type RuntimeSetup = (
    Arc<::tools::ToolExecutor>,
    Arc<dyn runtime::AgentModel>,
    Option<Arc<dyn runtime::SandboxFactory>>,
);
pub type RuntimeFactory = fn(&str, Arc<EventBus>) -> RuntimeSetup;

pub fn default_factory(_: &str, bus: Arc<EventBus>) -> RuntimeSetup {
    (
        Arc::new(::tools::ToolExecutor::new(bus)),
        Arc::new(NeverModel),
        None,
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
        let state = state_root(&root);
        let sidebar = sidebar_path(&root);
        // The existing exclusion, not a new registry: shared maintenance and the
        // writer leases the disposal context retains.
        let maintenance =
            storage::shared_with_wait(&config.db_path, &state, std::time::Duration::ZERO).unwrap();
        let leases = Arc::new(
            storage::WriterLeases::acquire(&config.db_path, &state, std::time::Duration::ZERO)
                .unwrap(),
        );
        let storage = Storage::open_with_writer_lease(
            config.clone(),
            maintenance.db_lease().unwrap(),
            leases.db_lease(),
        )
        .unwrap();
        assert_eq!(
            runtime::thread_retention::binding::ensure_binding(
                &Database::open(&config).unwrap(),
                &storage.handle(),
                &leases.authority().unwrap(),
                runtime::thread_retention::BindingPaths {
                    ownership_root: &state,
                    sidebar_path: &sidebar,
                },
                &config.db_path,
                1,
            )
            .unwrap(),
            runtime::thread_retention::BindingStatus::Bound
        );
        let bus = Arc::new(EventBus::new(64));
        let (tools, model, sandbox) = factory(&case, bus.clone());
        let runtime = match sandbox {
            Some(factory) => AgentRuntime::with_sandbox_factory(bus, tools, model, factory),
            None => AgentRuntime::new(bus, tools, model),
        }
        .with_run_store(RunStore::open(&config, storage.handle()).unwrap());
        let scratch = Arc::new(ThreadScratch::at(root.join("scratch")));
        runtime.set_thread_scratch(scratch.clone());
        runtime.set_disposal_context(leases.clone(), state, sidebar);
        runtime
            .adopt_restart_handoff(proof)
            .unwrap_or_else(|e| panic!("DB Adopt failed: {}", e.error));
        let f = Fixture {
            root,
            runtime,
            storage,
            config,
            scratch,
            leases,
            _maintenance: maintenance,
            _instance: instance,
        };
        assert_eq!(f.tx().phase, RestartPhase::Adopted);
        check(&f, &case);
    });
}
