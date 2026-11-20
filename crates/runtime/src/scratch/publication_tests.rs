//! Snapshot + auxiliary observation -> real publication -> conditional reset.
use super::*;
use crate::ownership::Lease;
type Notices = (
    HashMap<String, u64>,
    HashMap<String, u64>,
    HashMap<RunId, (String, u64)>,
    HashMap<RunId, (String, u64)>,
);
fn notices(scratch: &ThreadScratch) -> Notices {
    let s = scratch.lock();
    (
        s.resets.clone(),
        s.archived.clone(),
        s.seen.clone(),
        s.archive_seen.clone(),
    )
}
pub(crate) struct Observation {
    pub(crate) scratch: Arc<ThreadScratch>,
    thread: String,
    epoch: u64,
    lease: Lease,
    before: Notices,
    _dir: tempfile::TempDir,
}
impl Observation {
    pub(crate) fn new(runtime: &AgentRuntime, thread: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let scratch = Arc::new(ThreadScratch::at(dir.path().join("scratch")));
        let lease = Lease {
            owner_id: "owner".into(),
            generation: 1,
            expires_at: u64::MAX,
        };
        scratch.record_scratch_lease(thread, &lease);
        let key = ScratchKey::Thread(thread.into());
        std::fs::write(scratch.prepare(&key).unwrap().join("keep"), "keep").unwrap();
        assert!(!scratch.take_emptied_notice(&key, RunId::new(u64::MAX), true));
        assert!(!scratch.take_archived_notice(&key, RunId::new(u64::MAX)));
        let (epoch, _) = scratch.archive_snapshot(thread).unwrap();
        let before = notices(&scratch);
        runtime.set_thread_scratch(scratch.clone());
        Self {
            scratch,
            thread: thread.into(),
            epoch,
            lease,
            before,
            _dir: dir,
        }
    }
    pub(crate) fn assert_preserved(&self) {
        // All publication effects have completed, without yielding to a model
        // loop. Only the epoch hook can invalidate this unused observation.
        assert_eq!(notices(&self.scratch), self.before);
        let effect = GraphDisposalKey::new(
            "graph".into(),
            "tx".into(),
            "capture".into(),
            self.thread.clone(),
            1,
        )
        .unwrap();
        let outcome = self
            .scratch
            .reset_thread_if_unused(&self.thread, self.epoch, &self.lease, &effect)
            .unwrap();
        assert_eq!(
            outcome, None,
            "publication must invalidate the prior auxiliary observation"
        );
        assert_eq!(
            std::fs::read(self.scratch.thread_dir(&self.thread).join("keep")).unwrap(),
            b"keep"
        );
        assert_eq!(notices(&self.scratch), self.before);
    }
}
fn runtime() -> AgentRuntime {
    let bus = Arc::new(event_bus::EventBus::new(64));
    AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(crate::escalation_review::support::ScriptedModel::new([])),
    )
}
#[test]
fn bind_publication_invalidates_unused_scratch_observation() {
    let runtime = runtime();
    let run = runtime.reserve_run_id();
    let observation = Observation::new(&runtime, "thread");
    runtime.bind_thread_root("thread", run).unwrap();
    observation.assert_preserved();
}
#[test]
fn snapshot_root_publication_invalidates_unused_scratch_observation() {
    for old in [true, false] {
        let runtime = runtime();
        let run = runtime.reserve_run_id();
        runtime.bind_thread_root("old", run).unwrap();
        let mut goal = runtime
            .create_thread_goal("old", run, "objective".into(), vec!["evidence".into()])
            .unwrap();
        goal.thread_id = "thread".into();
        goal.root_run_id = runtime.reserve_run_id().to_string();
        let observation = Observation::new(&runtime, if old { "old" } else { "thread" });
        runtime.restore_thread_goal(goal).unwrap();
        observation.assert_preserved();
    }
}
#[test]
fn handoff_root_publication_invalidates_both_old_and_new_threads() {
    for old in [true, false] {
        let runtime = runtime();
        let source = runtime.reserve_run_id();
        let target = runtime.reserve_run_id();
        runtime.bind_thread_root("source", source).unwrap();
        let thread = event_bus::escalation_thread_id(&target.to_string());
        let observation = Observation::new(&runtime, if old { "source" } else { &thread });
        runtime
            .handoff_thread_goal(source, target, None, |_| {})
            .unwrap();
        observation.assert_preserved();
    }
}
#[test]
fn committed_handoff_publication_invalidates_unused_scratch_observation() {
    use storage::handoffs::{HandoffReceipt, HandoffState};
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("runs.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let runtime =
        runtime().with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    let source = runtime.reserve_run_id();
    let target = runtime.reserve_run_id();
    let handle = storage.handle();
    handle
        .reserve_handoff(HandoffReceipt {
            source_run_id: source.to_string(),
            source_incarnation: "source-incarnation".into(),
            target_run_id: target.to_string(),
            lane_id: "lane".into(),
            request_json: "{}".into(),
            state: HandoffState::Preparing,
            goal_json: None,
        })
        .unwrap();
    for (from, to) in [
        (HandoffState::Preparing, HandoffState::Prepared),
        (HandoffState::Prepared, HandoffState::Committed),
    ] {
        handle
            .advance_handoff(
                source.to_string(),
                "source-incarnation".into(),
                from,
                to,
                None,
            )
            .unwrap();
    }
    let thread = event_bus::escalation_thread_id(&target.to_string());
    let observation = Observation::new(&runtime, &thread);
    runtime.restore_committed_handoff(target).unwrap();
    observation.assert_preserved();
    let unset =
        self::runtime().with_run_store(crate::RunStore::open(&config, storage.handle()).unwrap());
    unset.restore_committed_handoff(target).unwrap();
    assert!(unset.shared.thread_scratch.lock().unwrap().is_none());
}
#[test]
fn unset_runtime_publication_does_not_initialize_scratch() {
    let runtime = runtime();
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    let goal = runtime
        .create_thread_goal("thread", root, "objective".into(), vec!["evidence".into()])
        .unwrap();
    runtime.restore_thread_goal(goal).unwrap();
    runtime
        .handoff_thread_goal(root, runtime.reserve_run_id(), None, |_| {})
        .unwrap();
    assert!(runtime.shared.thread_scratch.lock().unwrap().is_none());
}
