use super::*;
use crate::memory_queue::LearningSettings;
use crate::producer::ProducerGate;
use crate::{
    AgentInvocationContext, AgentModel, AgentRunPhase, ModelPreference, Role, RunConfig, RunStore,
    RuntimeError,
};
use providers::{ChatResponse, ContentBlock, FinishReason, Message, ToolSpec, Usage};
use std::collections::HashMap;
use std::time::{Duration, UNIX_EPOCH};
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
    assert!(queue.start_attempt("t1", &permit).unwrap().is_some());
    assert!(
        !queue.start_attempt("t1", &permit).unwrap().is_some(),
        "start is a single CAS"
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Started);

    let key = queue.job("t1").unwrap().unwrap().key();
    let key = queue
        .change(
            &key,
            LearningAttemptChange::BindRun {
                run: "extract".into(),
                review: false,
            },
        )
        .unwrap()
        .unwrap();
    let key = queue
        .change(&key, LearningAttemptChange::ExtractionReady(vec![]))
        .unwrap()
        .unwrap();
    assert!(
        queue
            .change(&key, LearningAttemptChange::Publish)
            .unwrap()
            .is_some()
    );
    assert_eq!(fixture.state("t1"), LearningJobState::Finished);
    assert!(!fixture.has_evidence("t1"));
    assert!(
        queue
            .change(
                &key,
                LearningAttemptChange::Fail(LearningEndReason::PipelineFailed)
            )
            .unwrap()
            .is_none(),
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
    assert!(!other.start_attempt("t1", &permit).unwrap().is_some());
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
            assert!(queue.start_attempt(task, &permit).unwrap().is_some());
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
            assert!(queue.start_attempt(task, &permit).unwrap().is_some());
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
            assert!(queue.start_attempt(task, &permit).unwrap().is_some());
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

#[test]
fn origins_made_in_one_process_share_an_instance_and_never_look_dead_to_each_other() {
    let core = current_origin(LearningOriginKind::Core, Some("cfg".into()));
    let gui = current_origin(LearningOriginKind::Gui, None);
    assert_eq!(core.instance, gui.instance);
    let probe = ProcOriginProbe::new(&core);
    assert_eq!(probe.liveness(&gui), OriginLiveness::Alive);
    assert_eq!(probe.liveness(&core), OriginLiveness::Alive);
}

#[tokio::test]
async fn a_live_process_never_sweeps_or_adopts_its_own_other_origins_jobs() {
    let fixture = Fixture::new();
    let core = fixture.queue(current_origin(LearningOriginKind::Core, Some("cfg".into())));
    let gui = fixture.queue(current_origin(LearningOriginKind::Gui, None));
    let sibling = fixture.queue(current_origin(LearningOriginKind::Core, Some("cfg".into())));
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    gui.admit(admit_request("gui-pending", "run-1")).unwrap();
    gui.admit(admit_request("gui-started", "run-2")).unwrap();
    assert!(gui.start_attempt("gui-started", &permit).unwrap().is_some());
    sibling
        .admit(admit_request("core-pending", "run-3"))
        .unwrap();
    let probe = ProcOriginProbe::new(core.origin());

    assert_eq!(
        core.sweep_dead_origins(&probe).unwrap(),
        SweepReport::default()
    );
    assert!(core.rescan_core(&probe).unwrap().is_empty());

    assert_eq!(fixture.state("gui-pending"), LearningJobState::Pending);
    assert_eq!(fixture.state("gui-started"), LearningJobState::Started);
    assert_eq!(fixture.state("core-pending"), LearningJobState::Pending);
    assert!(fixture.has_evidence("gui-started"));
}

/// Predecessor half of the exec test. It leaves a pending and a started job under its
/// origin and then replaces its own image, keeping the pid and the start time.
#[cfg(unix)]
#[test]
fn exec_predecessor() {
    use std::os::unix::process::CommandExt;
    let Ok(db_path) = std::env::var("EVORCH_LEARNING_JOBS_EXEC_DB") else {
        return;
    };
    let config = StorageConfig {
        db_path: db_path.into(),
        ..Default::default()
    };
    {
        let storage = Storage::open(config.clone()).unwrap();
        let gate = ProducerGate::default();
        let permit = gate.try_enter().unwrap();
        let queue = LearningJobQueue::new(
            storage.handle(),
            config.clone(),
            current_origin(LearningOriginKind::Core, Some("cfg".into())),
        );
        queue.admit(admit_request("exec-pending", "run-1")).unwrap();
        queue.admit(admit_request("exec-started", "run-2")).unwrap();
        assert!(
            queue
                .start_attempt("exec-started", &permit)
                .unwrap()
                .is_some()
        );
    }
    let error = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "learning_jobs::tests::exec_successor",
            "--nocapture",
        ])
        .exec();
    panic!("exec failed: {error}");
}

/// Successor half: the same pid and start time as the predecessor, a new image.
#[cfg(unix)]
#[test]
fn exec_successor() {
    let Ok(db_path) = std::env::var("EVORCH_LEARNING_JOBS_EXEC_DB") else {
        return;
    };
    let config = StorageConfig {
        db_path: db_path.into(),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let me = current_origin(LearningOriginKind::Core, Some("cfg".into()));
    let queue = LearningJobQueue::new(storage.handle(), config.clone(), me.clone());
    let stored = queue.job("exec-pending").unwrap().unwrap();
    assert_eq!(stored.origin.pid, me.pid, "exec keeps the pid");
    assert_eq!(
        stored.origin.start_ticks, me.start_ticks,
        "and the start time"
    );
    assert_ne!(stored.origin.instance, me.instance, "but not the image");

    let probe = ProcOriginProbe::new(&me);
    assert_eq!(probe.liveness(&stored.origin), OriginLiveness::Dead);
    let adopted = queue.rescan_core(&probe).unwrap();
    let tasks: Vec<_> = adopted.iter().map(|job| job.job.task_id.as_str()).collect();
    assert_eq!(tasks, ["exec-pending"]);
    let report = queue.sweep_dead_origins(&probe).unwrap();
    assert_eq!(report.abandoned_started, 1);
    assert_eq!(
        queue.job("exec-started").unwrap().unwrap().state,
        LearningJobState::Abandoned
    );
}

#[cfg(unix)]
#[tokio::test]
async fn an_exec_successor_takes_over_its_predecessors_pending_jobs_only() {
    let fixture = Fixture::new();
    let status = tokio::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "learning_jobs::tests::exec_predecessor",
            "--nocapture",
        ])
        .env("EVORCH_LEARNING_JOBS_EXEC_DB", &fixture.config.db_path)
        .status()
        .await
        .unwrap();
    assert!(status.success(), "the successor's own assertions failed");
    assert_eq!(fixture.state("exec-pending"), LearningJobState::Pending);
    assert_eq!(fixture.state("exec-started"), LearningJobState::Abandoned);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_starts_of_one_job_succeed_exactly_once() {
    let fixture = Fixture::new();
    let queue = fixture.queue(origin(LearningOriginKind::Core, "a", Some("cfg")));
    queue.admit(admit_request("t1", "run-1")).unwrap();
    let gate = ProducerGate::default();
    let racers = 8;
    let barrier = Arc::new(tokio::sync::Barrier::new(racers));
    let mut handles = Vec::new();
    for _ in 0..racers {
        let (queue, gate, barrier) = (queue.clone(), gate.clone(), Arc::clone(&barrier));
        handles.push(tokio::spawn(async move {
            let permit = gate.try_enter().unwrap();
            barrier.wait().await;
            queue.start_attempt("t1", &permit).unwrap().is_some()
        }));
    }
    let mut started = 0;
    for handle in handles {
        started += usize::from(handle.await.unwrap());
    }
    assert_eq!(started, 1);
    assert_eq!(fixture.state("t1"), LearningJobState::Started);
}

/// Child of the cross-process race: waits for the parent's go-ahead on stdin (the
/// barrier), then admits the shared task, rescans for the dead core's job and starts
/// what it won. A no-op unless the parent asked for it.
#[test]
fn race_child() {
    use std::io::{BufRead, Write};
    let Ok(db_path) = std::env::var("EVORCH_LEARNING_JOBS_RACE_DB") else {
        return;
    };
    let config = StorageConfig {
        db_path: db_path.into(),
        ..Default::default()
    };
    let storage = Storage::open(config.clone()).unwrap();
    let queue = LearningJobQueue::new(
        storage.handle(),
        config,
        current_origin(LearningOriginKind::Core, Some("cfg".into())),
    );
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    println!("READY");
    std::io::stdout().flush().unwrap();
    let mut go = String::new();
    std::io::stdin().lock().read_line(&mut go).unwrap();

    if std::env::var("EVORCH_LEARNING_JOBS_RACE_ACTION").as_deref() == Ok("janitor") {
        queue
            .sweep_dead_origins(&FakeProbe::new(&[("old", OriginLiveness::Dead)]))
            .unwrap();
        println!("RESULT admitted=0 adopted=0 started=0");
        std::io::stdout().flush().unwrap();
        return;
    }
    let admitted = queue
        .admit(admit_request("race-task", "race-root"))
        .unwrap()
        == LearningAdmission::Admitted;
    let probe = FakeProbe::new(&[("old", OriginLiveness::Dead)]);
    let adopted = queue.rescan_core(&probe).unwrap();
    let mut started = 0;
    for job in &adopted {
        started += usize::from(
            queue
                .start_attempt(&job.job.task_id, &permit)
                .unwrap()
                .is_some(),
        );
    }
    println!(
        "RESULT admitted={} adopted={} started={started}",
        u8::from(admitted),
        adopted.len()
    );
    std::io::stdout().flush().unwrap();
}

#[tokio::test]
async fn independent_processes_racing_admission_adoption_and_janitor_win_exactly_once() {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let fixture = Fixture::new();
    fixture
        .queue(origin(LearningOriginKind::Core, "old", Some("cfg")))
        .admit(admit_request("dead-pending", "run-0"))
        .unwrap();
    let exe = std::env::current_exe().unwrap();
    let mut children = Vec::new();
    for action in ["adopt", "adopt", "janitor"] {
        let mut child = tokio::process::Command::new(&exe)
            .args(["--exact", "learning_jobs::tests::race_child", "--nocapture"])
            .env("EVORCH_LEARNING_JOBS_RACE_DB", &fixture.config.db_path)
            .env("EVORCH_LEARNING_JOBS_RACE_ACTION", action)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap()).lines();
        children.push((child, stdout));
    }
    // All three children report READY before any gets the go-ahead, so their
    // admissions and rescans overlap instead of running one after the other.
    for (_, stdout) in &mut children {
        loop {
            let line = stdout
                .next_line()
                .await
                .unwrap()
                .expect("child exited early");
            if line == "READY" {
                break;
            }
        }
    }
    for (child, _) in &mut children {
        let stdin = child.stdin.as_mut().unwrap();
        stdin.write_all(b"go\n").await.unwrap();
        stdin.flush().await.unwrap();
    }
    let mut totals = [0_usize; 3];
    for (child, stdout) in &mut children {
        let mut result = None;
        while let Some(line) = stdout.next_line().await.unwrap() {
            if let Some(rest) = line.strip_prefix("RESULT ") {
                result = Some(rest.to_owned());
            }
        }
        assert!(child.wait().await.unwrap().success());
        let result = result.expect("child printed a result");
        for (slot, field) in ["admitted", "adopted", "started"].iter().enumerate() {
            let value = result
                .split_whitespace()
                .find_map(|part| part.strip_prefix(&format!("{field}=")))
                .expect("field present");
            totals[slot] += value.parse::<usize>().unwrap();
        }
    }
    assert_eq!(totals, [1, 1, 1], "admitted, adopted, started");
    assert_eq!(fixture.state("race-task"), LearningJobState::Pending);
    assert_eq!(fixture.state("dead-pending"), LearningJobState::Started);
}

#[tokio::test]
async fn learning_ack_loss_reads_the_exact_operation_and_never_speculates_a_start() {
    use std::sync::atomic::Ordering;
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "ack-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    let permit = runtime.learning_gate().try_enter().unwrap();
    queue.admit(admit_request("before", "before-root")).unwrap();
    let entry = runtime
        .shared
        .learning_executions
        .register("before", runtime.shared.learning_jobs_changed.clone())
        .unwrap();
    queue.ack_fault.store(1, Ordering::SeqCst);
    assert!(entry.start(&queue, &permit).is_err());
    runtime.learning_returned(&entry).await;
    let ended = queue.job("before").unwrap().unwrap();
    assert_eq!(
        (ended.state, ended.attempt),
        (LearningJobState::Abandoned, 0)
    );
    assert_eq!(ended.reason.as_deref(), Some("unconfirmed_write"));
    assert!(!f.has_evidence("before"));
    assert!(runtime.list_agents().is_empty());
    queue.admit(admit_request("after", "after-root")).unwrap();
    let before = queue.job("after").unwrap().unwrap().key();
    queue.ack_fault.store(2, Ordering::SeqCst);
    let started = queue
        .change(&before, LearningAttemptChange::Start)
        .unwrap()
        .unwrap();
    assert_eq!(
        (started.attempt, started.revision),
        (1, before.revision + 1)
    );
    let bound = queue
        .change(
            &started,
            LearningAttemptChange::BindRun {
                run: "fresh".into(),
                review: false,
            },
        )
        .unwrap()
        .unwrap();
    assert!(bound.revision > started.revision);
    assert_eq!(
        queue
            .receipt(&before, &LearningAttemptChange::Start)
            .unwrap(),
        Some(started)
    );
    let mut foreign = before.clone();
    foreign.owner = "foreign".into();
    assert!(
        queue
            .receipt(&foreign, &LearningAttemptChange::Start)
            .unwrap()
            .is_none()
    );
    assert!(
        queue
            .change(&before, LearningAttemptChange::Start)
            .unwrap()
            .is_none()
    );
    assert_eq!(queue.job("after").unwrap().unwrap().attempt, 1);
}

#[tokio::test]
async fn learning_unknown_commit_readback_ends_without_candidates_after_normal_return() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "readback-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let mut queue = f.queue(owner);
    queue
        .admit(admit_request("unconfirmed", "unconfirmed-root"))
        .unwrap();
    let entry = runtime
        .shared
        .learning_executions
        .register("unconfirmed", runtime.shared.learning_jobs_changed.clone())
        .unwrap();
    let permit = runtime.learning_gate().try_enter().unwrap();
    assert!(entry.start(&queue, &permit).unwrap());
    assert!(
        entry
            .change(
                &queue,
                LearningAttemptChange::BindRun {
                    run: "extract".into(),
                    review: false
                }
            )
            .unwrap()
    );
    queue.storage.db_path = f._dir.path().join("not-a-directory").join("unavailable.db");
    // Real writer commit; only independent readback is unavailable.
    assert!(
        entry
            .change(&queue, LearningAttemptChange::ExtractionReady(Vec::new()))
            .is_err()
    );
    runtime.learning_returned(&entry).await;
    let job = Database::open(&f.config)
        .unwrap()
        .learning_job("unconfirmed")
        .unwrap()
        .unwrap();
    assert_eq!((job.state, job.attempt), (LearningJobState::Abandoned, 1));
    assert_eq!(job.reason.as_deref(), Some("unconfirmed_write"));
    assert_eq!(job.publication.as_deref(), Some("None"));
    assert!(!f.has_evidence("unconfirmed"));
    assert!(runtime.list_agents().is_empty());
}

#[tokio::test(start_paused = true)]
async fn learning_late_cleanup_does_not_reopen_a_later_pause_or_retry_before_return() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "late-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    queue.admit(admit_request("late", "late-root")).unwrap();
    let entry = runtime
        .shared
        .learning_executions
        .register("late", runtime.shared.learning_jobs_changed.clone())
        .unwrap();
    {
        let permit = runtime.learning_gate().try_enter().unwrap();
        assert!(entry.start(&queue, &permit).unwrap());
    }
    let mut changed = runtime.subscribe_learning_changes();
    let pause = runtime.pause_learning();
    let suspension = Arc::new(runtime.suspend_learning(&pause).unwrap());
    loop {
        if queue.job("late").unwrap().unwrap().state == LearningJobState::Interrupted {
            break;
        }
        changed.changed().await.unwrap();
    }
    assert_eq!(
        suspension
            .wait_until(tokio::time::Instant::now())
            .await
            .unwrap(),
        crate::learning_suspend::LearningSuspendWait::DeadlineExceeded
    );
    runtime
        .cancel_learning_suspension(pause, suspension)
        .unwrap();
    assert_eq!(
        queue.job("late").unwrap().unwrap().state,
        LearningJobState::Interrupted
    );
    let later = runtime.pause_learning();
    // This explicit receipt stands in for a root scope held by an owned task;
    // the full journal/dispatch path is exercised by post_run_learning.
    runtime.learning_returned(&entry).await;
    loop {
        if queue.job("late").unwrap().unwrap().state == LearningJobState::Pending {
            break;
        }
        changed.changed().await.unwrap();
    }
    assert!(!runtime.learning_gate().is_open());
    assert_eq!(queue.job("late").unwrap().unwrap().attempt, 1);
    assert!(runtime.list_agents().is_empty());
    assert!(!runtime.learning_gate().is_open());
    drop(later);
}

#[tokio::test]
async fn learning_interrupted_core_jobs_are_reserved_for_exclusive_recovery_not_shared_janitor() {
    let f = Fixture::new();
    let gate = ProducerGate::default();
    let permit = gate.try_enter().unwrap();
    for (kind, id, cfg) in [
        (LearningOriginKind::Core, "dead-core", "cfg"),
        (LearningOriginKind::Core, "other-config", "other"),
        (LearningOriginKind::Gui, "dead-gui", "cfg"),
        (LearningOriginKind::Gui, "live-gui", "cfg"),
        (LearningOriginKind::Gui, "unknown-gui", "cfg"),
    ] {
        let queue = f.queue(origin(kind, id, Some(cfg)));
        queue.admit(admit_request(id, id)).unwrap();
        let key = queue.start_attempt(id, &permit).unwrap().unwrap();
        queue
            .change(
                &key,
                LearningAttemptChange::Interrupt {
                    suspension_id: "planned".into(),
                },
            )
            .unwrap()
            .unwrap();
    }
    let current = f.queue(origin(LearningOriginKind::Core, "new", Some("cfg")));
    let probe = FakeProbe::new(&[
        ("dead-core", OriginLiveness::Dead),
        ("other-config", OriginLiveness::Dead),
        ("dead-gui", OriginLiveness::Dead),
        ("live-gui", OriginLiveness::Alive),
    ]);
    assert!(current.rescan_core(&probe).unwrap().is_empty());
    current.sweep_dead_origins(&probe).unwrap();
    for id in ["dead-core", "other-config", "live-gui", "unknown-gui"] {
        assert_eq!(f.state(id), LearningJobState::Interrupted);
        assert!(f.has_evidence(id));
    }
    assert_eq!(f.state("dead-gui"), LearningJobState::Abandoned);
    assert_eq!(
        current.job("dead-gui").unwrap().unwrap().reason.as_deref(),
        Some(LearningEndReason::OriginDied.as_str())
    );
    assert!(!f.has_evidence("dead-gui"));
    assert!(f.bare_runtime().list_agents().is_empty());
}

#[tokio::test(start_paused = true)]
async fn learning_rejected_terminal_write_keeps_cleanup_owned_until_it_commits() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "terminal-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    queue
        .admit(admit_request("rejected", "rejected-root"))
        .unwrap();
    let entry = runtime
        .shared
        .learning_executions
        .register("rejected", runtime.shared.learning_jobs_changed.clone())
        .unwrap();
    {
        let permit = runtime.learning_gate().try_enter().unwrap();
        assert!(entry.start(&queue, &permit).unwrap());
    }
    let injected = rusqlite::Connection::open(&f.config.db_path).unwrap();
    injected
        .execute_batch(
            "CREATE TRIGGER inject_terminal_rollback BEFORE UPDATE ON learning_jobs
             WHEN NEW.state IN ('Finished','Failed')
             BEGIN SELECT RAISE(ABORT, 'injected rollback'); END;",
        )
        .unwrap();
    assert!(
        entry
            .change(
                &queue,
                LearningAttemptChange::Fail(LearningEndReason::PipelineFailed)
            )
            .is_err()
    );
    // The pipeline returns with the terminal write still rejected: the entry is not
    // released, so the job is neither forgotten nor reported as cleaned up.
    runtime.learning_returned(&entry).await;
    assert_eq!(f.state("rejected"), LearningJobState::Started);
    assert!(f.has_evidence("rejected"));
    let pause = runtime.pause_learning();
    let suspension = runtime.suspend_learning(&pause).unwrap();
    let deadline = || tokio::time::Instant::now() + Duration::from_secs(60);
    // The unresolved terminal write is reported as an error and never as a clean return.
    assert!(suspension.wait_until(deadline()).await.is_err());
    assert_eq!(f.state("rejected"), LearningJobState::Started);

    injected
        .execute_batch("DROP TRIGGER inject_terminal_rollback;")
        .unwrap();
    runtime.reconcile_learning_suspension(&suspension).await;
    assert_eq!(
        suspension.wait_until(deadline()).await.unwrap(),
        crate::learning_suspend::LearningSuspendWait::Complete
    );
    let ended = queue.job("rejected").unwrap().unwrap();
    assert_eq!(ended.state, LearningJobState::Failed);
    assert_eq!(
        ended.reason.as_deref(),
        Some(LearningEndReason::PipelineFailed.as_str())
    );
    assert!(!f.has_evidence("rejected"));
    assert!(runtime.list_agents().is_empty());
}

/// A started job whose terminal write the database keeps rolling back, returned with the
/// write still rejected. The returned connection holds the injected trigger.
async fn returned_with_rejected_terminal(
    f: &Fixture,
    task: &str,
) -> (
    AgentRuntime,
    Arc<crate::learning_suspend::LearningExecution>,
    rusqlite::Connection,
) {
    let owner = origin(LearningOriginKind::Core, "retry-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    queue.admit(admit_request(task, "retry-root")).unwrap();
    let entry = runtime
        .shared
        .learning_executions
        .register(task, runtime.shared.learning_jobs_changed.clone())
        .unwrap();
    {
        let permit = runtime.learning_gate().try_enter().unwrap();
        assert!(entry.start(&queue, &permit).unwrap());
    }
    let injected = rusqlite::Connection::open(&f.config.db_path).unwrap();
    injected
        .execute_batch(
            "CREATE TRIGGER inject_terminal_rollback BEFORE UPDATE ON learning_jobs
             WHEN NEW.state IN ('Finished','Failed')
             BEGIN SELECT RAISE(ABORT, 'injected rollback'); END;",
        )
        .unwrap();
    assert!(
        entry
            .change(
                &queue,
                LearningAttemptChange::Fail(LearningEndReason::PipelineFailed)
            )
            .is_err()
    );
    runtime.learning_returned(&entry).await;
    assert_eq!(f.state(task), LearningJobState::Started);
    (runtime, entry, injected)
}

async fn returned_cleanly(entry: &crate::learning_suspend::LearningExecution) {
    let mut receipt = entry.returned_receipt();
    while !matches!(*receipt.borrow_and_update(), Some(Ok(()))) {
        receipt.changed().await.unwrap();
    }
}

// The database recovers after the pipeline and its first reconcile both failed. Nobody
// calls suspend or reconcile: the runtime's own retry settles the job.
#[tokio::test(start_paused = true)]
async fn learning_unresolved_terminal_is_retried_by_the_runtime_once_the_database_recovers() {
    let f = Fixture::new();
    let (runtime, entry, injected) = returned_with_rejected_terminal(&f, "retried").await;
    injected
        .execute_batch("DROP TRIGGER inject_terminal_rollback;")
        .unwrap();

    returned_cleanly(&entry).await;

    let ended = f.queue(origin(LearningOriginKind::Core, "retry-owner", Some("cfg")));
    let ended = ended.job("retried").unwrap().unwrap();
    assert_eq!(ended.state, LearningJobState::Failed);
    assert_eq!(
        ended.reason.as_deref(),
        Some(LearningEndReason::PipelineFailed.as_str())
    );
    assert!(!f.has_evidence("retried"));
    assert!(runtime.list_agents().is_empty());
}

// The backoff is capped, not abandoned: the entry stays owned through many failed attempts
// and still settles when the database finally recovers.
#[tokio::test(start_paused = true)]
async fn learning_unresolved_terminal_keeps_retrying_past_the_backoff_cap() {
    let f = Fixture::new();
    let (runtime, entry, injected) = returned_with_rejected_terminal(&f, "capped").await;
    let mut attempts = runtime.subscribe_learning_changes();
    for _ in 0..9 {
        attempts.changed().await.unwrap();
        assert_eq!(f.state("capped"), LearningJobState::Started);
        assert!(f.has_evidence("capped"));
        assert!(matches!(*entry.returned_receipt().borrow(), Some(Err(_))));
    }
    injected
        .execute_batch("DROP TRIGGER inject_terminal_rollback;")
        .unwrap();

    returned_cleanly(&entry).await;

    assert_eq!(f.state("capped"), LearningJobState::Failed);
    assert!(!f.has_evidence("capped"));
}

// Cancelling a two-job suspension while the database rolls back the requeue and the settling
// write: the first job's cleanup stays owned and unresolved, no new attempt can take the task, and the second job stays reserved.
// Once the database recovers the runtime settles the first job and the cancel moves on to
// requeue the second one, with no explicit reconcile.
#[tokio::test(start_paused = true)]
async fn learning_cancel_requeues_later_jobs_once_an_older_entrys_readback_recovers() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "cancel-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    let mut entries = Vec::new();
    for task in ["first", "second"] {
        queue
            .admit(admit_request(task, &format!("{task}-root")))
            .unwrap();
        let entry = runtime
            .shared
            .learning_executions
            .register(task, runtime.shared.learning_jobs_changed.clone())
            .unwrap();
        let permit = runtime.learning_gate().try_enter().unwrap();
        assert!(entry.start(&queue, &permit).unwrap());
        entries.push(entry);
    }
    let mut changes = runtime.subscribe_learning_changes();
    let pause = runtime.pause_learning();
    let suspension = Arc::new(runtime.suspend_learning(&pause).unwrap());
    while ["first", "second"]
        .iter()
        .any(|task| f.state(task) != LearningJobState::Interrupted)
    {
        changes.changed().await.unwrap();
    }
    for entry in &entries {
        runtime.learning_returned(entry).await;
    }
    suspension.wait().await.unwrap();
    let broken = rusqlite::Connection::open(&f.config.db_path).unwrap();
    broken
        .execute_batch(
            "CREATE TRIGGER inject_cancel_rollback BEFORE UPDATE ON learning_jobs
             WHEN NEW.state IN ('Pending','Abandoned')
             BEGIN SELECT RAISE(ABORT, 'injected rollback'); END;",
        )
        .unwrap();

    runtime
        .cancel_learning_suspension(pause, suspension)
        .unwrap();
    let mut stuck = entries[0].returned_receipt();
    while !matches!(*stuck.borrow_and_update(), Some(Err(_))) {
        stuck.changed().await.unwrap();
    }
    assert_eq!(f.state("second"), LearningJobState::Interrupted);
    assert!(
        runtime
            .shared
            .learning_executions
            .register("first", runtime.shared.learning_jobs_changed.clone())
            .is_none(),
        "an unresolved cleanup keeps its task"
    );

    broken
        .execute_batch("DROP TRIGGER inject_cancel_rollback;")
        .unwrap();
    while f.state("second") == LearningJobState::Interrupted {
        changes.changed().await.unwrap();
    }

    assert_ne!(f.state("first"), LearningJobState::Interrupted);
    assert_ne!(f.state("second"), LearningJobState::Abandoned);
    returned_cleanly(&entries[0]).await;
}

// A dispatch that merely registers must not make an old Publish cleanup look finished: the old
// entry keeps its slot, the job stays Started with its evidence, and only the committed
// terminal write releases it.
#[tokio::test(start_paused = true)]
async fn learning_a_refused_registration_does_not_settle_an_unresolved_publish_cleanup() {
    let f = Fixture::new();
    let (runtime, entry, injected) = returned_with_rejected_terminal(&f, "held").await;

    for _ in 0..2 {
        assert!(
            runtime
                .shared
                .learning_executions
                .register("held", runtime.shared.learning_jobs_changed.clone())
                .is_none()
        );
    }
    assert!(matches!(*entry.returned_receipt().borrow(), Some(Err(_))));
    assert_eq!(f.state("held"), LearningJobState::Started);
    assert!(f.has_evidence("held"));

    injected
        .execute_batch("DROP TRIGGER inject_terminal_rollback;")
        .unwrap();
    returned_cleanly(&entry).await;
    assert_eq!(f.state("held"), LearningJobState::Failed);
    assert!(!f.has_evidence("held"));
}

// An entry with no attempt (its intake was never confirmed) may end only the Pending job it
// observed. A job that a newer attempt already started is never written over.
#[tokio::test(start_paused = true)]
async fn learning_unconfirmed_intake_cleanup_never_ends_a_job_it_did_not_observe_pending() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "intake-owner", Some("cfg"));
    let runtime = f.runtime(owner.clone());
    let queue = f.queue(owner);
    for task in ["running", "pending"] {
        queue
            .admit(admit_request(task, &format!("{task}-root")))
            .unwrap();
    }
    let started = queue.job("running").unwrap().unwrap();
    queue
        .change(&started.key(), LearningAttemptChange::Start)
        .unwrap()
        .unwrap();
    for task in ["running", "pending"] {
        let entry = runtime
            .shared
            .learning_executions
            .register(task, runtime.shared.learning_jobs_changed.clone())
            .unwrap();
        entry.intake_unconfirmed();
        runtime.learning_returned(&entry).await;
        returned_cleanly(&entry).await;
    }

    assert_eq!(f.state("running"), LearningJobState::Started);
    assert!(f.has_evidence("running"));
    let ended = queue.job("pending").unwrap().unwrap();
    assert_eq!(ended.state, LearningJobState::Abandoned);
    assert_eq!(
        ended.reason.as_deref(),
        Some(LearningEndReason::UnconfirmedWrite.as_str())
    );
    assert!(!f.has_evidence("pending"));
}

/// Runs `hook` with the instance of the first judged origin when the second origin is judged,
/// which is after the first job was adopted and before the rescan resumes it.
struct SyncProbe<F: Fn(&str) + Send + Sync> {
    judged: std::sync::Mutex<Vec<String>>,
    hook: F,
}
impl<F: Fn(&str) + Send + Sync> OriginProbe for SyncProbe<F> {
    fn liveness(&self, origin: &LearningOrigin) -> OriginLiveness {
        let mut judged = self.judged.lock().unwrap();
        if judged.len() == 1 {
            (self.hook)(&judged[0]);
        }
        judged.push(origin.instance.clone());
        OriginLiveness::Dead
    }
}

#[tokio::test]
async fn learning_rescan_never_ends_a_job_the_scheduler_started_after_its_adoption() {
    let f = Fixture::new();
    let worker = f.bare_runtime();
    let source = worker.delegate_background(Role::Worker, "work".into(), RunConfig::default());
    assert_eq!(worker.wait(source).await.unwrap(), AgentRunPhase::Done);
    let evidence = worker
        .freeze_run_learning(source)
        .unwrap()
        .to_json()
        .unwrap();
    for name in ["a", "b"] {
        let mut request = admit_request(&format!("job-{name}"), &source.to_string());
        request.roots = vec![format!("root-{name}")];
        request.evidence_json = evidence.clone();
        f.queue(origin(
            LearningOriginKind::Core,
            &format!("old-{name}"),
            Some("cfg"),
        ))
        .admit(request)
        .unwrap();
    }
    let owner = origin(LearningOriginKind::Core, "new", Some("cfg"));
    let successor = f.runtime(owner.clone());
    let queue = f.queue(owner);
    let started = std::sync::Mutex::new(Vec::new());
    let probe = SyncProbe {
        judged: std::sync::Mutex::default(),
        hook: |first: &str| {
            // The scheduler wins the race for the job adopted first: it registers and
            // starts it while the rescan is still judging the next origin.
            let task = format!("job-{}", first.trim_start_matches("old-"));
            let entry = successor
                .shared
                .learning_executions
                .register(&task, successor.shared.learning_jobs_changed.clone())
                .unwrap();
            let permit = successor.learning_gate().try_enter().unwrap();
            assert!(entry.start(&queue, &permit).unwrap());
            started.lock().unwrap().push((task, entry));
        },
    };
    let resumed = successor.rescan_pending_learning(&probe).unwrap();

    let started = started.into_inner().unwrap();
    assert_eq!(started.len(), 1);
    let (raced, _entry) = &started[0];
    // The raced job keeps running under the scheduler's attempt, evidence included.
    assert_eq!(f.state(raced), LearningJobState::Started);
    assert!(f.has_evidence(raced));
    // Only the other job was resumed by the rescan itself.
    assert_eq!(resumed.len(), 1);
    assert_ne!(&resumed[0].task_id, raced);
}

#[tokio::test]
async fn learning_abandon_observed_only_ends_the_pending_key_it_saw() {
    let f = Fixture::new();
    let owner = origin(LearningOriginKind::Core, "observer", Some("cfg"));
    let queue = f.queue(owner);
    let permit = ProducerGate::default().try_enter().unwrap();
    queue.admit(admit_request("seen", "seen-root")).unwrap();
    let observed = queue.job("seen").unwrap().unwrap().key();
    assert!(queue.start_attempt("seen", &permit).unwrap().is_some());
    assert!(!queue.abandon_observed(&observed).unwrap());
    assert_eq!(f.state("seen"), LearningJobState::Started);
    assert!(f.has_evidence("seen"));
}
