use super::*;
use crate::agent_loop::{LoopState, delegate_cleanup_tests::started_fixture};
use std::sync::{
    Barrier,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use tools::{ShellJobSnapshot, ShellJobState, Tool, ToolError, ToolExecutor, ToolResult};

type SnapshotPause = (mpsc::Sender<()>, Arc<Barrier>);
type ReleaseProbe = Box<dyn Fn() + Send + Sync>;

pub(crate) struct Shell {
    jobs: Mutex<Vec<ShellJobSnapshot>>,
    sender: Mutex<Option<tokio::sync::watch::Sender<u64>>>,
    receiver: tokio::sync::watch::Receiver<u64>,
    pause: Mutex<Option<SnapshotPause>>,
    pub(crate) release_probe: Mutex<Option<ReleaseProbe>>,
    pub(crate) released: AtomicUsize,
}

#[async_trait::async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    fn permissions(&self) -> tools::Permissions {
        tools::Permissions::read_only()
    }
    async fn execute(&self, _: serde_json::Value) -> Result<ToolResult, ToolError> {
        unreachable!("this fixture only observes shell lifecycle")
    }
    fn shell_job_snapshots(&self, _: &str) -> Vec<ShellJobSnapshot> {
        let jobs = self.jobs.lock().unwrap().clone();
        let pause = self.pause.lock().unwrap().take();
        if let Some((entered, resume)) = pause {
            entered.send(()).unwrap();
            resume.wait();
        }
        jobs
    }
    fn subscribe_shell_job_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        Some(self.receiver.clone())
    }
    fn release_shell_jobs(&self, _: &str) -> Result<(), ToolError> {
        if let Some(probe) = self.release_probe.lock().unwrap().take() {
            probe();
        }
        self.jobs.lock().unwrap().clear();
        self.released.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

pub(crate) struct Harness {
    pub(crate) state: LoopState,
    pub(crate) scratch: Arc<ThreadScratch>,
    pub(crate) key: ScratchKey,
    pub(crate) shell: Arc<Shell>,
    _runtime: AgentRuntime,
    _temp: tempfile::TempDir,
}

impl Harness {
    pub(crate) async fn new() -> Self {
        Self::build(false).await
    }
    async fn build(unknown_teardown: bool) -> Self {
        let (_cancel, receiver) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (runtime, mut state) = started_fixture(receiver).await;
        let run = state.task.run_id;
        // Obtain a genuine opaque identity through the tools API, without a
        // public identity constructor or production test-only capability.
        let bus = Arc::new(event_bus::EventBus::new(32));
        let sandbox = Arc::new(sandbox::DirectSandbox::new_unchecked());
        let real = ToolExecutor::with_standard_tools(bus.clone(), sandbox);
        let result = real
            .execute(
                &tools::ToolExecutionContext {
                    run_id: run.to_string(),
                    ..Default::default()
                },
                "shell",
                "identity",
                if unknown_teardown {
                    // Pass the executable preflight, but fail the admitted spawn.
                    serde_json::json!({"command": "invalid\0command"})
                } else {
                    serde_json::json!({"command": ":", "yield_ms": 0})
                },
            )
            .await;
        if unknown_teardown {
            assert!(result.is_err());
        } else {
            result.unwrap();
        }
        let snapshot = real.shell_job_snapshots(&run.to_string()).pop().unwrap();
        let snapshot = if unknown_teardown {
            assert_eq!(snapshot.state, ShellJobState::Terminal);
            assert!(
                real.wait_shell_job_terminal(&snapshot.identity)
                    .await
                    .is_err()
            );
            snapshot
        } else {
            real.wait_shell_job_terminal(&snapshot.identity)
                .await
                .unwrap()
        };
        assert_eq!(snapshot.teardown_confirmed, !unknown_teardown);
        let (sender, receiver) = tokio::sync::watch::channel(0);
        let shell = Arc::new(Shell {
            jobs: Mutex::new(vec![snapshot]),
            sender: Mutex::new(Some(sender)),
            receiver,
            pause: Mutex::new(None),
            release_probe: Mutex::new(None),
            released: AtomicUsize::new(0),
        });
        let mut executor = ToolExecutor::new(bus);
        executor.register(shell.clone()).unwrap();
        state.shared.executor = Arc::new(executor);
        let temp = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(temp.path().join("scratch")));
        let key = ScratchKey::Thread("thread".into());
        let dir = scratch.prepare(&key).unwrap();
        std::fs::write(dir.join("keep"), "keep").unwrap();
        let incarnation = state.task.publication.incarnation();
        scratch.register_execution(&key, run, &incarnation).unwrap();
        let mut binding = RunScratch::new(dir, false);
        binding.binding = Some((scratch.clone(), key.clone(), run, incarnation));
        state.scratch = Some(binding);
        Self {
            state,
            scratch,
            key,
            shell,
            _runtime: runtime,
            _temp: temp,
        }
    }
    pub(crate) fn terminal(&self, terminal: bool) {
        let mut jobs = self.shell.jobs.lock().unwrap();
        jobs[0].state = if terminal {
            ShellJobState::Terminal
        } else {
            ShellJobState::Running
        };
        jobs[0].teardown_confirmed = terminal;
    }
    pub(crate) fn has_shell_users(&self) -> bool {
        has_shell_users(&self.scratch, &self.key)
    }
    pub(crate) fn has_users(&self) -> bool {
        !self.scratch.lock().usage[&self.key].users.is_empty()
    }
}

#[tokio::test]
async fn missing_shell_record_requires_supervisor_teardown_proof_not_absence() {
    for unknown in [false, true] {
        let h = Harness::build(unknown).await;
        h.terminal(false);
        h.state.sync_scratch_shell_users();
        assert!(h.has_shell_users());
        h.shell.jobs.lock().unwrap().clear();
        h.state.sync_scratch_shell_users();
        assert_eq!(h.has_shell_users(), unknown);
        h.state.release_scratch_usage();
        assert_eq!(h.has_users(), unknown);
        assert_eq!(
            std::fs::read(h.scratch.dir(&h.key).join("keep")).unwrap(),
            b"keep"
        );
    }
}

pub(crate) fn has_shell_users(scratch: &ThreadScratch, key: &ScratchKey) -> bool {
    scratch.lock().usage[key]
        .users
        .iter()
        .any(|user| matches!(user, ScratchUser::Shell { .. }))
}

// Isolate the idle boundary: the normal model loop refuses to end a turn with
// unobserved running jobs, so a scripted background job cannot reach that state.
// Here the real LoopState monitor and archive worker exercise the independent
// shell lifetime without changing that unrelated finish guard.
#[tokio::test]
async fn shell_teardown_notification_reclaims_after_the_idle_boundary_without_a_new_hint() {
    let mut h = Harness::new().await;
    let sockets = test_socket_dir::SocketDir::new();
    let host = crate::ownership::OwnerHost::open(
        sockets.path(),
        Default::default(),
        Arc::new(event_bus::EventBus::new(32)),
    )
    .unwrap();
    host.start("thread").unwrap();
    let scratch = host.thread_scratch();
    let run = h.state.task.run_id;
    let incarnation = h.state.task.publication.incarnation();
    scratch
        .register_execution(&h.key, run, &incarnation)
        .unwrap();
    let dir = scratch.prepare(&h.key).unwrap();
    std::fs::write(dir.join("keep"), "keep").unwrap();
    let mut binding = RunScratch::new(dir.clone(), false);
    binding.binding = Some((scratch.clone(), h.key.clone(), run, incarnation));
    h.state.scratch = Some(binding);
    h.scratch = scratch;
    let (consulted, mut received) = tokio::sync::mpsc::unbounded_channel();
    host.set_scratch_archive_checker(Arc::new(move |_| {
        consulted.send(()).unwrap();
        Ok(true)
    }));
    h.terminal(false);
    h.state.start_scratch_monitor();
    h.state.release_scratch_usage();
    host.hint_scratch_archive("thread");
    let pass = host.scratch_archive_barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert!(received.try_recv().is_err());
    assert!(h.has_shell_users());
    assert_eq!(std::fs::read(dir.join("keep")).unwrap(), b"keep");
    h.terminal(true);
    h.shell
        .sender
        .lock()
        .unwrap()
        .as_ref()
        .unwrap()
        .send_modify(|revision| *revision += 1);
    // No new hint or input. Only the monitor's teardown notification wakes the
    // pending archive, and the barrier proves that its rename finished.
    received.recv().await.unwrap();
    let pass = host.scratch_archive_barrier();
    tokio::task::spawn_blocking(move || pass.recv().unwrap())
        .await
        .unwrap();
    assert!(!h.has_users());
    assert!(!dir.exists());
}

#[tokio::test]
async fn closed_shell_observation_retains_identity_after_run_becomes_idle() {
    let mut h = Harness::new().await;
    h.state.start_scratch_monitor();
    drop(h.shell.sender.lock().unwrap().take());
    h.state
        .scratch
        .as_mut()
        .unwrap()
        .monitor
        .take()
        .unwrap()
        .await
        .unwrap();
    h.state.release_scratch_usage();
    let state = h.scratch.lock();
    assert!(
        state.usage[&h.key]
            .users
            .iter()
            .any(|user| matches!(user, ScratchUser::ObserverLost { .. }))
    );
    assert!(
        !state.usage[&h.key]
            .users
            .iter()
            .any(|user| matches!(user, ScratchUser::Run { .. }))
    );
    assert_eq!(
        std::fs::read(h.scratch.dir(&h.key).join("keep")).unwrap(),
        b"keep"
    );
}

#[tokio::test]
async fn older_running_snapshot_cannot_reinsert_a_confirmed_shell_user() {
    let h = Harness::new().await;
    h.terminal(false);
    let (entered, observed) = mpsc::channel();
    let resume = Arc::new(Barrier::new(2));
    *h.shell.pause.lock().unwrap() = Some((entered, resume.clone()));
    let old = std::thread::spawn({
        let scratch = h.scratch.clone();
        let executor = h.state.shared.executor.clone();
        let key = h.key.clone();
        let run = h.state.task.run_id;
        move || scratch.sync_shell_users(&key, run, &executor)
    });
    observed.recv().unwrap();
    // The requirement is this specific gate: do not release it between
    // observing an immutable job snapshot and applying that observation.
    let gate_held = h.scratch.state.try_lock().is_err();
    h.terminal(true);
    let new = std::thread::spawn({
        let scratch = h.scratch.clone();
        let executor = h.state.shared.executor.clone();
        let key = h.key.clone();
        let run = h.state.task.run_id;
        move || scratch.sync_shell_users(&key, run, &executor)
    });
    resume.wait();
    old.join().unwrap();
    new.join().unwrap();
    assert!(
        gate_held,
        "the old snapshot must hold the scratch usage gate"
    );
    assert!(!h.has_shell_users(), "confirmed teardown cannot be undone");
}

#[test]
fn execution_release_forgets_its_incarnation_but_preserves_the_epoch_on_resume() {
    let temp = tempfile::tempdir().unwrap();
    let scratch = ThreadScratch::at(temp.path().join("scratch"));
    let key = ScratchKey::Thread("thread".into());
    let run = RunId::new(1);
    let lease = Lease {
        owner_id: "owner".into(),
        generation: 1,
        expires_at: u64::MAX,
    };
    scratch.record_scratch_lease("thread", &lease);
    let path = scratch.prepare(&key).unwrap();
    for incarnation in ["old", "current"] {
        scratch.register_execution(&key, run, incarnation).unwrap();
    }
    scratch.release_execution(run, "old");
    {
        let state = scratch.lock();
        assert_eq!(state.execution_keys.len(), 1);
        assert_eq!(
            state.execution_keys.get(&(run, "current".into())),
            Some(&key)
        );
    }
    scratch.release_execution(run, "old");
    assert!(scratch.archive_snapshot("thread").is_none());
    scratch.release_execution(run, "current");
    assert!(scratch.lock().execution_keys.is_empty());
    let (epoch, lease) = scratch.archive_snapshot("thread").unwrap();
    scratch.register_execution(&key, run, "current").unwrap();
    assert_eq!(scratch.lock().execution_keys.len(), 1);
    assert!(scratch.archive_snapshot("thread").is_none());
    scratch.release_execution(run, "current");
    assert!(scratch.lock().execution_keys.is_empty());
    let (resumed_epoch, _) = scratch.archive_snapshot("thread").unwrap();
    assert_eq!(resumed_epoch, epoch + 1);
    // Even after the resumed user leaves, a snapshot from before that resume
    // cannot detach the tree. Removing the execution lookup must not reset it.
    assert!(
        scratch
            .detach_archived("thread", epoch, &lease)
            .unwrap()
            .is_none()
    );
    assert!(path.exists());
    assert_eq!(
        scratch
            .detach_archived("thread", resumed_epoch, &lease)
            .unwrap(),
        Some(true)
    );
    assert!(!path.exists());
}

#[test]
fn exhausted_epoch_or_notice_counter_keeps_the_tree_without_wrapping() {
    for epoch_exhausted in [true, false] {
        let temp = tempfile::tempdir().unwrap();
        let scratch = ThreadScratch::at(temp.path().join("scratch"));
        let key = ScratchKey::Thread("thread".into());
        let lease = Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        };
        scratch.record_scratch_lease("thread", &lease);
        let path = scratch.prepare(&key).unwrap();
        std::fs::write(path.join("keep"), "keep").unwrap();
        if epoch_exhausted {
            scratch.lock().usage.entry(key.clone()).or_default().epoch = u64::MAX;
            assert!(
                scratch
                    .register_execution(&key, RunId::new(1), "incarnation")
                    .is_err()
            );
            assert!(scratch.archive_snapshot("thread").is_none());
            assert!(
                scratch
                    .detach_archived("thread", u64::MAX, &lease)
                    .unwrap()
                    .is_none()
            );
            assert_eq!(scratch.lock().usage[&key].epoch, u64::MAX);
        } else {
            scratch.lock().archived.insert(key.name(), u64::MAX);
            assert!(scratch.detach_archived("thread", 0, &lease).is_err());
            assert_eq!(scratch.lock().archived[&key.name()], u64::MAX);
        }
        assert_eq!(std::fs::read_to_string(path.join("keep")).unwrap(), "keep");
    }
}
