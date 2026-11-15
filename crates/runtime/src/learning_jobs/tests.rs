use super::*;
use crate::memory_queue::LearningSettings;
use crate::producer::ProducerGate;
use crate::{
    AgentInvocationContext, AgentModel, AgentRunPhase, ModelPreference, Role, RunConfig, RunStore,
    RuntimeError,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use std::collections::HashMap;
use std::time::UNIX_EPOCH;
use storage::{Storage, StorageConfig};

struct StopModel;
#[async_trait::async_trait]
impl AgentModel for StopModel {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        Ok(ChatResponse {
            usage_reported: true,
            message: Message {
                role: providers::Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        })
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "fixture".into()
    }
}

fn origin(kind: LearningOriginKind, instance: &str, config: Option<&str>) -> LearningOrigin {
    LearningOrigin {
        kind,
        instance: instance.into(),
        pid: 4242,
        start_ticks: Some(7),
        boot_id: Some("boot".into()),
        pid_ns: Some("pidns".into()),
        config_identity: config.map(str::to_string),
    }
}

/// Judges by instance; anything not listed cannot be judged.
struct FakeProbe(HashMap<String, OriginLiveness>);
impl FakeProbe {
    fn new(entries: &[(&str, OriginLiveness)]) -> Self {
        Self(
            entries
                .iter()
                .map(|(instance, state)| ((*instance).to_string(), *state))
                .collect(),
        )
    }
}
impl OriginProbe for FakeProbe {
    fn liveness(&self, origin: &LearningOrigin) -> OriginLiveness {
        self.0
            .get(&origin.instance)
            .copied()
            .unwrap_or(OriginLiveness::Unknown)
    }
}

fn admit_request(task: &str, root: &str) -> AdmitRequest {
    AdmitRequest {
        task_id: task.into(),
        roots: vec![root.into()],
        goal: false,
        daily_limit: 20,
        project: "p".into(),
        source_run_id: root.into(),
        prompt: "prompt".into(),
        evidence_json: "{\"records\":[]}".into(),
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    storage: Storage,
    config: StorageConfig,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("jobs.db"),
            ..Default::default()
        };
        let storage = Storage::open(config.clone()).unwrap();
        Self {
            _dir: dir,
            storage,
            config,
        }
    }

    fn queue(&self, origin: LearningOrigin) -> LearningJobQueue {
        LearningJobQueue::new(self.storage.handle(), self.config.clone(), origin)
            .with_clock(|| UNIX_EPOCH + Duration::from_secs(86_400 * 20_000))
    }

    fn state(&self, task: &str) -> LearningJobState {
        Database::open(&self.config)
            .unwrap()
            .learning_job(task)
            .unwrap()
            .expect("job exists")
            .state
    }

    fn has_evidence(&self, task: &str) -> bool {
        Database::open(&self.config)
            .unwrap()
            .learning_job_evidence(task)
            .unwrap()
            .is_some()
    }

    /// A runtime that runs work but has no learning of its own, so a finished run
    /// does not admit a job by itself.
    fn bare_runtime(&self) -> AgentRuntime {
        let bus = Arc::new(event_bus::EventBus::new(128));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        AgentRuntime::new(bus, executor, Arc::new(StopModel))
            .with_sequential_run_ids()
            .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap())
    }

    fn runtime(&self, origin: LearningOrigin) -> AgentRuntime {
        let bus = Arc::new(event_bus::EventBus::new(128));
        let executor = Arc::new(tools::ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        ));
        AgentRuntime::new(bus, executor, Arc::new(StopModel))
            .with_sequential_run_ids()
            .with_run_store(RunStore::open(&self.config, self.storage.handle()).unwrap())
            .with_learning(LearningSettings {
                writer: self.storage.handle(),
                storage: self.config.clone(),
                project: "p".into(),
                quick: ModelPreference {
                    profile: "quick".into(),
                    model: None,
                    reasoning_effort: None,
                },
            })
            .with_learning_origin(origin)
    }
}

#[tokio::test]
async fn a_job_goes_pending_started_finished_and_its_evidence_goes_with_it() {
    let fixture = Fixture::new();
    let queue = fixture.queue(origin(LearningOriginKind::Core, "a", Some("cfg")));
    let gate = ProducerGate::default();

    assert_eq!(
        queue.admit(admit_request("t1", "run-1")).unwrap(),
        LearningAdmission::Admitted
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Pending);
    assert!(fixture.has_evidence("t1"));
    assert_eq!(
        queue.admit(admit_request("t1", "run-1")).unwrap(),
        LearningAdmission::Duplicate
    );

    let permit = gate.try_enter().unwrap();
    assert!(queue.start("t1", &permit).unwrap());
    assert!(
        !queue.start("t1", &permit).unwrap(),
        "start is a single CAS"
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Started);

    assert!(queue.finish("t1", true).unwrap());
    assert_eq!(fixture.state("t1"), LearningJobState::Finished);
    assert!(!fixture.has_evidence("t1"));
    assert!(
        !queue.finish("t1", false).unwrap(),
        "an ended job stays ended"
    );
}

#[tokio::test]
async fn only_the_admitting_incarnation_can_start_or_end_its_job() {
    let fixture = Fixture::new();
    let owner = fixture.queue(origin(LearningOriginKind::Core, "a", Some("cfg")));
    let other = fixture.queue(origin(LearningOriginKind::Core, "b", Some("cfg")));
    let gate = ProducerGate::default();
    owner.admit(admit_request("t1", "run-1")).unwrap();

    let permit = gate.try_enter().unwrap();
    assert!(!other.start("t1", &permit).unwrap());
    assert!(!other.abandon("t1", "not mine").unwrap());
    assert_eq!(fixture.state("t1"), LearningJobState::Pending);
}

#[tokio::test]
async fn rescan_takes_only_pending_jobs_of_a_dead_core_of_the_same_config() {
    let fixture = Fixture::new();
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    let mine = fixture.queue(origin(LearningOriginKind::Core, "me", Some("cfg")));
    let cases = [
        // (instance, kind, config, task)
        (
            "dead-core",
            LearningOriginKind::Core,
            Some("cfg"),
            "dead-core",
        ),
        (
            "dead-other-cfg",
            LearningOriginKind::Core,
            Some("elsewhere"),
            "other-config",
        ),
        ("dead-gui", LearningOriginKind::Gui, None, "dead-gui"),
        (
            "live-core",
            LearningOriginKind::Core,
            Some("cfg"),
            "live-core",
        ),
        (
            "unknown-core",
            LearningOriginKind::Core,
            Some("cfg"),
            "unknown-core",
        ),
        (
            "dead-started",
            LearningOriginKind::Core,
            Some("cfg"),
            "dead-started",
        ),
    ];
    for (index, (instance, kind, config, task)) in cases.iter().enumerate() {
        let queue = fixture.queue(origin(*kind, instance, *config));
        queue
            .admit(admit_request(task, &format!("run-{index}")))
            .unwrap();
        if *task == "dead-started" {
            assert!(queue.start(task, &permit).unwrap());
        }
    }
    let probe = FakeProbe::new(&[
        ("dead-core", OriginLiveness::Dead),
        ("dead-other-cfg", OriginLiveness::Dead),
        ("dead-gui", OriginLiveness::Dead),
        ("live-core", OriginLiveness::Alive),
        ("dead-started", OriginLiveness::Dead),
    ]);

    let adopted = mine.rescan_core(&probe).unwrap();

    let tasks: Vec<_> = adopted.iter().map(|job| job.job.task_id.as_str()).collect();
    assert_eq!(tasks, ["dead-core"]);
    assert_eq!(adopted[0].job.origin.instance, "me");
    assert_eq!(adopted[0].evidence.prompt, "prompt");
    for untouched in ["other-config", "dead-gui", "live-core", "unknown-core"] {
        assert_eq!(
            fixture.state(untouched),
            LearningJobState::Pending,
            "{untouched}"
        );
    }
    assert_eq!(fixture.state("dead-started"), LearningJobState::Started);
    assert!(
        mine.rescan_core(&probe).unwrap().is_empty(),
        "a taken-over job is not taken twice"
    );
}

#[tokio::test]
async fn a_gui_never_rescans() {
    let fixture = Fixture::new();
    let gui = fixture.queue(origin(LearningOriginKind::Gui, "gui", None));
    assert!(matches!(
        gui.rescan_core(&FakeProbe::new(&[])),
        Err(LearningQueueError::NotCore)
    ));
    let configless = fixture.queue(origin(LearningOriginKind::Core, "core", None));
    assert!(matches!(
        configless.rescan_core(&FakeProbe::new(&[])),
        Err(LearningQueueError::NotCore)
    ));
}

#[tokio::test]
async fn the_janitor_ends_dead_work_without_running_anything_and_spares_the_rest() {
    let fixture = Fixture::new();
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    let janitor = fixture.queue(origin(LearningOriginKind::Gui, "janitor", None));
    let jobs = [
        ("started-dead", LearningOriginKind::Core, true),
        ("gui-dead", LearningOriginKind::Gui, false),
        ("core-dead", LearningOriginKind::Core, false),
        ("started-alive", LearningOriginKind::Gui, true),
        ("pending-unknown", LearningOriginKind::Gui, false),
    ];
    for (index, (task, kind, started)) in jobs.iter().enumerate() {
        let config = (*kind == LearningOriginKind::Core).then_some("cfg");
        let queue = fixture.queue(origin(*kind, task, config));
        queue
            .admit(admit_request(task, &format!("run-{index}")))
            .unwrap();
        if *started {
            assert!(queue.start(task, &permit).unwrap());
        }
    }
    let probe = FakeProbe::new(&[
        ("started-dead", OriginLiveness::Dead),
        ("gui-dead", OriginLiveness::Dead),
        ("core-dead", OriginLiveness::Dead),
        ("started-alive", OriginLiveness::Alive),
    ]);

    let report = janitor.sweep_dead_origins(&probe).unwrap();

    assert_eq!(
        report,
        SweepReport {
            abandoned_started: 1,
            abandoned_gui_pending: 1,
            left_for_core: 1,
            origin_alive: 1,
            origin_unknown: 1,
        }
    );
    assert_eq!(fixture.state("started-dead"), LearningJobState::Abandoned);
    assert!(
        !fixture.has_evidence("started-dead"),
        "a started job is never re-run, so its evidence goes"
    );
    assert_eq!(fixture.state("gui-dead"), LearningJobState::Abandoned);
    assert!(!fixture.has_evidence("gui-dead"));
    assert_eq!(fixture.state("core-dead"), LearningJobState::Pending);
    assert!(fixture.has_evidence("core-dead"));
    assert_eq!(fixture.state("started-alive"), LearningJobState::Started);
    assert_eq!(fixture.state("pending-unknown"), LearningJobState::Pending);
}

#[test]
fn a_proc_probe_judges_liveness_from_start_ticks_and_never_guesses() {
    // Field 22 is the start time; the command name may hold spaces and parentheses.
    let stat = |pid: u32, ticks: u64| {
        format!(
            "{pid} (a b) c) S {} {ticks} 0 0\n",
            "0 ".repeat(18).trim_end()
        )
    };
    let proc_root = tempfile::tempdir().unwrap();
    let write = |pid: u32, ticks: u64| {
        let dir = proc_root.path().join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("stat"), stat(pid, ticks)).unwrap();
    };
    let me = origin(LearningOriginKind::Core, "me", Some("cfg"));
    let probe = ProcOriginProbe::with_proc_root(&me, proc_root.path().to_path_buf());
    let theirs = |pid: u32, ticks: Option<u64>| LearningOrigin {
        pid,
        start_ticks: ticks,
        ..origin(LearningOriginKind::Core, "them", Some("cfg"))
    };
    write(100, 7);
    write(200, 8);

    assert_eq!(probe.liveness(&me), OriginLiveness::Alive);
    assert_eq!(probe.liveness(&theirs(100, Some(7))), OriginLiveness::Alive);
    assert_eq!(
        probe.liveness(&theirs(100, Some(6))),
        OriginLiveness::Dead,
        "pid reused"
    );
    assert_eq!(
        probe.liveness(&theirs(300, Some(7))),
        OriginLiveness::Dead,
        "no such process"
    );
    assert_eq!(probe.liveness(&theirs(100, None)), OriginLiveness::Unknown);
    // Same pid and start time as this process under another instance: it exec'd.
    assert_eq!(
        probe.liveness(&theirs(me.pid, me.start_ticks)),
        OriginLiveness::Dead
    );

    let other_boot = LearningOrigin {
        boot_id: Some("older".into()),
        ..theirs(100, Some(7))
    };
    assert_eq!(probe.liveness(&other_boot), OriginLiveness::Dead);
    let no_boot = LearningOrigin {
        boot_id: None,
        ..theirs(100, Some(7))
    };
    assert_eq!(probe.liveness(&no_boot), OriginLiveness::Unknown);
    let other_ns = LearningOrigin {
        pid_ns: Some("elsewhere".into()),
        ..theirs(100, Some(7))
    };
    assert_eq!(probe.liveness(&other_ns), OriginLiveness::Unknown);
}

#[tokio::test]
async fn a_drain_that_hits_its_limit_times_out_and_kills_nothing() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(origin(LearningOriginKind::Core, "a", Some("cfg")));
    let queue = runtime.learning_job_queue().unwrap();
    queue.admit(admit_request("t1", "run-1")).unwrap();
    let permit = runtime.learning_gate().enter().await.unwrap();
    assert!(queue.start("t1", &permit).unwrap());

    let drain = runtime.begin_learning_drain();
    let outcome = drain.wait_until(std::future::ready(())).await;

    assert_eq!(outcome, LearningDrainOutcome::TimedOut);
    assert_eq!(
        runtime.learning_gate().active(),
        1,
        "the pipeline keeps its permit"
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Started);

    // The pipeline ends on its own; a later wait sees it.
    assert!(queue.finish("t1", true).unwrap());
    drop(permit);
    assert_eq!(
        drain.wait_until(std::future::pending::<()>()).await,
        LearningDrainOutcome::Drained
    );
}

#[tokio::test]
async fn a_drain_blocks_new_starts_but_leaves_their_jobs_pending_until_resumed() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(origin(LearningOriginKind::Core, "a", Some("cfg")));
    let queue = runtime.learning_job_queue().unwrap();
    queue.admit(admit_request("t1", "run-1")).unwrap();

    let drain = runtime.begin_learning_drain();
    assert!(runtime.learning_gate().try_enter().is_none());
    assert_eq!(
        drain.wait_until(std::future::pending::<()>()).await,
        LearningDrainOutcome::Drained
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Pending);
    assert!(fixture.has_evidence("t1"));
    assert!(
        runtime.producer_gate().try_enter().is_some(),
        "the learning pause does not stop the shared producer gate"
    );

    assert!(drain.resume());
    assert!(runtime.learning_gate().try_enter().is_some());
}

#[tokio::test]
async fn a_drain_fails_rather_than_report_drained_over_a_started_job_nobody_runs() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime(origin(LearningOriginKind::Core, "a", Some("cfg")));
    let queue = runtime.learning_job_queue().unwrap();
    queue.admit(admit_request("t1", "run-1")).unwrap();
    let permit = runtime.learning_gate().enter().await.unwrap();
    assert!(queue.start("t1", &permit).unwrap());
    drop(permit);

    let drain = runtime.begin_learning_drain();
    assert!(matches!(
        drain.wait_until(std::future::pending::<()>()).await,
        LearningDrainOutcome::Failed(_)
    ));

    runtime.learning_gate().close();
    assert!(matches!(
        drain.wait_until(std::future::pending::<()>()).await,
        LearningDrainOutcome::Failed(_)
    ));
}

#[tokio::test]
async fn a_core_runs_a_dead_cores_pending_job_from_its_stored_evidence() {
    let fixture = Fixture::new();
    let worker = fixture.bare_runtime();
    let source = worker.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    assert_eq!(worker.wait(source).await.unwrap(), AgentRunPhase::Done);
    let mut request = admit_request("handover", &source.to_string());
    request.evidence_json = worker
        .freeze_run_learning(source)
        .unwrap()
        .to_json()
        .unwrap();
    fixture
        .queue(origin(LearningOriginKind::Core, "old", Some("cfg")))
        .admit(request)
        .unwrap();

    let successor = fixture.runtime(origin(LearningOriginKind::Core, "new", Some("cfg")));
    let probe = FakeProbe::new(&[("old", OriginLiveness::Dead)]);
    let mut resumed = successor.rescan_pending_learning(&probe).unwrap();

    assert_eq!(resumed.len(), 1);
    let mut completion = resumed.remove(0).completion;
    let result = completion
        .wait_for(|result| result.is_some())
        .await
        .unwrap()
        .clone()
        .unwrap();
    // The extractor of this fixture submits nothing, so the pipeline may not finish
    // cleanly; the job must still end, once, and drop its evidence.
    let expected = if result.is_ok() {
        LearningJobState::Finished
    } else {
        LearningJobState::Failed
    };
    assert_eq!(fixture.state("handover"), expected);
    assert!(!fixture.has_evidence("handover"));
}

/// Child half of the two-process test: admits jobs under real origins, then dies
/// without ending the started one. A no-op unless the parent asked for it.
#[test]
fn child_that_dies_owning_jobs() {
    let Ok(db_path) = std::env::var("EVORCH_LEARNING_JOBS_CHILD_DB") else {
        return;
    };
    let config = StorageConfig {
        db_path: db_path.into(),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    for (task, kind, started) in [
        ("child-core-pending", LearningOriginKind::Core, false),
        ("child-core-started", LearningOriginKind::Core, true),
        ("child-gui-pending", LearningOriginKind::Gui, false),
    ] {
        let config_identity = (kind == LearningOriginKind::Core).then(|| "cfg".to_string());
        let queue = LearningJobQueue::new(
            storage.handle(),
            config.clone(),
            current_origin(kind, config_identity),
        );
        queue.admit(admit_request(task, task)).unwrap();
        if started {
            assert!(queue.start(task, &permit).unwrap());
        }
    }
    std::process::exit(0);
}

#[tokio::test]
async fn a_core_recovers_the_jobs_of_a_process_that_really_died() {
    let fixture = Fixture::new();
    drop(fixture.storage.handle());
    let exe = std::env::current_exe().unwrap();
    // Waiting for the exit is the completion signal; the child is gone afterwards.
    let status = tokio::process::Command::new(exe)
        .args([
            "--exact",
            "learning_jobs::tests::child_that_dies_owning_jobs",
            "--nocapture",
        ])
        .env("EVORCH_LEARNING_JOBS_CHILD_DB", &fixture.config.db_path)
        .status()
        .await
        .unwrap();
    assert!(status.success());

    let me = current_origin(LearningOriginKind::Core, Some("cfg".into()));
    let probe = ProcOriginProbe::new(&me);
    let queue = fixture.queue(me);

    let adopted = queue.rescan_core(&probe).unwrap();
    let tasks: Vec<_> = adopted.iter().map(|job| job.job.task_id.as_str()).collect();
    assert_eq!(tasks, ["child-core-pending"]);

    let report = queue.sweep_dead_origins(&probe).unwrap();
    assert_eq!(report.abandoned_started, 1);
    assert_eq!(report.abandoned_gui_pending, 1);
    assert_eq!(
        fixture.state("child-core-started"),
        LearningJobState::Abandoned
    );
    assert_eq!(
        fixture.state("child-gui-pending"),
        LearningJobState::Abandoned
    );
    assert_eq!(
        fixture.state("child-core-pending"),
        LearningJobState::Pending
    );
}
