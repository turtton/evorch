//! Lanes in the Workbench (ADR 0044 Phase 1b): placement, family, repair from the ledger
//! and the stale-revision guards, driven by lane events and a ledger reader.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use event_bus::lane::{
    LaneBrief, LanePolicy as LedgerPolicy, LaneReport, LaneReportProducer, LaneStatus,
    LaneWorkspaceDeclaration,
};
use event_bus::{Event, LedgerEvent};
use storage::lanes::LaneRecord;
use workspace_ui::{LanePolicy, ThreadId};

use super::WorkbenchState;
use super::delivery_tests::state;
use super::lanes::{LaneLedgerReader, LaneSnapshot, Spawner};
use crate::fixture::DemoSource;
use crate::model::lanes::{LaneDetail, status_label};

/// Runs each ledger read on the calling thread, so a test steps the reads itself.
fn inline_spawner() -> Spawner {
    Arc::new(|job| job())
}

#[derive(Default)]
struct FakeLedger {
    lanes: Mutex<HashMap<String, LaneSnapshot>>,
    scans: Mutex<usize>,
}

impl FakeLedger {
    fn put(&self, snapshot: LaneSnapshot) {
        self.lanes
            .lock()
            .unwrap()
            .insert(snapshot.detail.record.thread_id.clone(), snapshot);
    }
}

impl LaneLedgerReader for FakeLedger {
    fn snapshot(&self, thread_id: &str) -> Result<Option<LaneSnapshot>, String> {
        Ok(self.lanes.lock().unwrap().get(thread_id).cloned())
    }

    fn scan(&self, _project_roots: &[PathBuf]) -> Result<Vec<LaneSnapshot>, String> {
        *self.scans.lock().unwrap() += 1;
        let lanes = self.lanes.lock().unwrap();
        let mut all: Vec<_> = lanes.values().cloned().collect();
        all.sort_by(|left, right| left.detail.record.lane_id.cmp(&right.detail.record.lane_id));
        Ok(all)
    }
}

fn record(n: u32, lead: Option<&str>, policy: LedgerPolicy, revision: u64) -> LaneRecord {
    LaneRecord {
        lane_id: format!("lane-{n}"),
        thread_id: format!("lane-thread-{n}"),
        origin_thread_id: "thread".into(),
        origin_run_id: "lead-run".into(),
        creation_key: format!("call-{n}"),
        scope: event_bus::lane::LaneScope::Lane,
        lead_thread_id: lead.map(str::to_owned),
        policy,
        management_revision: revision,
        brief: LaneBrief {
            objective: format!("Objective {n}\nsecond line"),
            inputs: vec![],
            deliverables: vec!["a patch".into()],
            acceptance_criteria: vec!["tests pass".into()],
            constraints: vec![],
            workspace: LaneWorkspaceDeclaration {
                branch: Some(format!("feat/lane-{n}")),
                worktree: Some(format!(".worktrees/lane-{n}")),
                notes: None,
            },
            report_policy: policy,
        },
        brief_revision: 1,
        start_seed: "seed".into(),
        start_blocked_reason: None,
        root_run_id: Some(format!("lane-root-{n}")),
        generation: 1,
        latest_report_seq: 0,
        created_at_ns: 1_000_000_000_000,
        updated_at_ns: 1_000_000_000_000,
        last_activity_at_ns: 1_000_000_000_000,
    }
}

fn report(n: u32, seq: u64, status: LaneStatus) -> LaneReport {
    LaneReport {
        lane_id: format!("lane-{n}"),
        report_seq: seq,
        status,
        summary: format!("report {seq}"),
        artifact_refs: vec![],
        needs_human: false,
        question_refs: vec![],
        reason_code: None,
        producer: LaneReportProducer::LaneAgent,
        management_revision: 1,
        brief_revision: 1,
        created_at_ns: 0,
    }
}

fn snapshot(record: LaneRecord, latest_report: Option<LaneReport>) -> LaneSnapshot {
    LaneSnapshot {
        project_id: Some("test".into()),
        title: None,
        created_at_ns: record.created_at_ns,
        detail: LaneDetail {
            record,
            latest_report,
        },
    }
}

fn workbench(ledger: Option<Arc<FakeLedger>>) -> (WorkbenchState<DemoSource>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let workbench = state(&dir.path().join("sidebar.json"));
    let workbench = match ledger {
        Some(ledger) => workbench
            .with_lane_ledger(ledger)
            .with_lane_spawner(inline_spawner()),
        None => workbench,
    };
    (workbench, dir)
}

fn created(record: &LaneRecord) -> Event {
    Event::new(record.created_event())
}

fn family(workbench: &WorkbenchState<DemoSource>, id: &str) -> String {
    workbench
        .sidebar
        .family_key(&ThreadId::new(id))
        .unwrap()
        .to_string()
}

fn ids(workbench: &WorkbenchState<DemoSource>, id: &str) -> usize {
    workbench
        .sidebar
        .threads
        .iter()
        .filter(|thread| thread.id.to_string() == id)
        .count()
}

#[test]
fn an_attached_lane_event_nests_the_lane_under_its_lead_without_any_read() {
    // Given: a lead and an attached lane event
    let (mut workbench, _dir) = workbench(None);
    let record = record(1, Some("thread"), LedgerPolicy::Attached, 1);

    // When: the event is folded
    assert!(workbench.apply_conversation_event(&created(&record)));

    // Then: the lane is a child of the lead, in the lead's family, as an Orchestrator
    let lane = workbench
        .sidebar
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == "lane-thread-1")
        .unwrap();
    assert_eq!(lane.tree_parent(), Some(&ThreadId::new("thread")));
    assert_eq!(
        lane.chat_role,
        Some(workspace_ui::ThreadChatRole::Orchestrator)
    );
    assert_eq!(family(&workbench, "lane-thread-1"), "thread");
}

#[test]
fn a_detached_lane_is_a_top_level_thread_with_a_family_of_its_own() {
    let (mut workbench, _dir) = workbench(None);
    let record = record(1, None, LedgerPolicy::Detached, 1);

    workbench.apply_conversation_event(&created(&record));
    workbench
        .sidebar
        .set_family_archived(&ThreadId::new("thread"), true, 10)
        .unwrap();

    assert_eq!(family(&workbench, "lane-thread-1"), "lane-thread-1");
    let lane = workbench
        .sidebar
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == "lane-thread-1")
        .unwrap();
    assert!(
        !lane.archived,
        "the lead's archive must not reach a detached lane"
    );
    assert_eq!(
        lane.lane.as_ref().unwrap().origin_thread_id,
        ThreadId::new("thread")
    );
}

#[test]
fn the_ledger_read_names_the_lane_and_shows_a_ready_report_as_unverified() {
    // Given: a lane whose latest report says ready
    let ledger = Arc::new(FakeLedger::default());
    let mut lane = record(1, Some("thread"), LedgerPolicy::Summary, 1);
    lane.latest_report_seq = 2;
    ledger.put(snapshot(
        lane.clone(),
        Some(report(1, 2, LaneStatus::Ready)),
    ));
    let (mut workbench, _dir) = workbench(Some(ledger));

    // When: the event and then the polled read are applied
    workbench.apply_conversation_event(&created(&lane));
    settle(&mut workbench);

    // Then: the thread is titled after the objective and the card words ready as unverified
    let title = &workbench
        .sidebar
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == "lane-thread-1")
        .unwrap()
        .title;
    assert_eq!(title, "Lane · Objective 1");
    let detail = workbench.lanes.details.get("lane-thread-1").unwrap();
    assert_eq!(detail.status(), Some(LaneStatus::Ready));
    assert_eq!(status_label(LaneStatus::Ready), "ready (unverified)");
    assert_eq!(detail.headline(), "Lane · summary · ready (unverified)");
    assert_eq!(
        detail.workspace_lines(),
        ["Branch: feat/lane-1", "Worktree: .worktrees/lane-1"]
    );
}

#[test]
fn a_lane_whose_event_was_lost_is_repaired_from_the_ledger_once() {
    // Given: two lanes in the ledger that the sidebar never heard of
    let ledger = Arc::new(FakeLedger::default());
    ledger.put(snapshot(
        record(1, Some("thread"), LedgerPolicy::Attached, 1),
        None,
    ));
    ledger.put(snapshot(record(2, None, LedgerPolicy::Detached, 1), None));
    let (mut workbench, _dir) = workbench(Some(Arc::clone(&ledger)));

    // When: the startup scan runs, then a late event and further polls repeat it
    settle(&mut workbench);
    workbench.apply_conversation_event(&created(&record(
        1,
        Some("thread"),
        LedgerPolicy::Attached,
        1,
    )));
    settle(&mut workbench);
    settle(&mut workbench);

    // Then: each lane is shown once, in its place, and the scan ran only once
    assert_eq!(ids(&workbench, "lane-thread-1"), 1);
    assert_eq!(ids(&workbench, "lane-thread-2"), 1);
    assert_eq!(family(&workbench, "lane-thread-1"), "thread");
    assert_eq!(family(&workbench, "lane-thread-2"), "lane-thread-2");
    assert_eq!(*ledger.scans.lock().unwrap(), 1);
}

#[test]
fn an_older_ledger_read_does_not_bring_a_detached_lane_back_under_its_lead() {
    // Given: a lane the sidebar knows as detached at revision 2
    let ledger = Arc::new(FakeLedger::default());
    ledger.put(snapshot(
        record(1, Some("thread"), LedgerPolicy::Attached, 1),
        None,
    ));
    let (mut workbench, _dir) = workbench(Some(ledger));
    workbench.apply_conversation_event(&Event::new(LedgerEvent::LaneCreated {
        lane_id: "lane-1".into(),
        thread_id: "lane-thread-1".into(),
        origin_run_id: "lead-run".into(),
        origin_thread_id: "thread".into(),
        lead_thread_id: None,
        policy: LedgerPolicy::Detached,
        management_revision: 2,
    }));

    // When: a read of the older revision arrives
    settle(&mut workbench);

    // Then: the lane stays detached
    assert_eq!(family(&workbench, "lane-thread-1"), "lane-thread-1");
}

#[test]
fn a_management_change_moves_the_lane_and_a_late_older_one_is_ignored() {
    let (mut workbench, _dir) = workbench(None);
    let attached = record(1, Some("thread"), LedgerPolicy::Attached, 1);
    workbench.apply_conversation_event(&created(&attached));
    let changed = |lead: Option<&str>, policy, revision| {
        Event::new(LedgerEvent::LaneManagementChanged {
            lane_id: "lane-1".into(),
            thread_id: "lane-thread-1".into(),
            run_id: None,
            lead_thread_id: lead.map(str::to_owned),
            policy,
            management_revision: revision,
        })
    };

    workbench.apply_conversation_event(&changed(None, LedgerPolicy::Detached, 2));
    assert_eq!(family(&workbench, "lane-thread-1"), "lane-thread-1");

    workbench.apply_conversation_event(&changed(Some("thread"), LedgerPolicy::Attached, 1));
    assert_eq!(family(&workbench, "lane-thread-1"), "lane-thread-1");
    assert_eq!(
        workbench
            .sidebar
            .threads
            .iter()
            .find(|t| t.id.to_string() == "lane-thread-1")
            .unwrap()
            .lane
            .as_ref()
            .unwrap()
            .policy,
        LanePolicy::Detached
    );
}

#[test]
fn a_lane_a_confirmed_deletion_removed_is_not_added_again() {
    let ledger = Arc::new(FakeLedger::default());
    ledger.put(snapshot(
        record(1, Some("thread"), LedgerPolicy::Attached, 1),
        None,
    ));
    let (workbench, _dir) = workbench(Some(ledger));
    let mut tombstones = storage::thread_retention::TombstoneSet::default();
    tombstones.threads.insert("lane-thread-1".into());
    let mut workbench = workbench.with_tombstones(tombstones);

    workbench.apply_conversation_event(&created(&record(
        1,
        Some("thread"),
        LedgerPolicy::Attached,
        1,
    )));
    settle(&mut workbench);

    assert_eq!(ids(&workbench, "lane-thread-1"), 0);
}

#[test]
fn a_report_raises_the_lane_in_the_lead_family_by_its_event_time() {
    // Given: two attached lanes, the second created later
    let (mut workbench, _dir) = workbench(None);
    for n in [1, 2] {
        workbench.apply_conversation_event(&created(&record(
            n,
            Some("thread"),
            LedgerPolicy::Attached,
            1,
        )));
    }
    let at = |n: u32| {
        workbench
            .sidebar
            .threads
            .iter()
            .find(|t| t.id.to_string() == format!("lane-thread-{n}"))
            .unwrap()
            .updated_at
    };
    let before = (at(1), at(2));

    // When: the first lane reports, 100 s after both
    let mut event = Event::new(LedgerEvent::LaneReportAppended {
        lane_id: "lane-1".into(),
        thread_id: "lane-thread-1".into(),
        run_id: None,
        report_seq: 1,
        status: LaneStatus::Working,
        needs_human: false,
    });
    event.meta.wall_clock = std::time::SystemTime::now() + std::time::Duration::from_secs(100);
    workbench.apply_conversation_event(&event);

    // Then: its activity moved forward and the other's did not
    assert!(
        workbench
            .sidebar
            .threads
            .iter()
            .any(|t| t.id.to_string() == "lane-thread-1" && t.updated_at >= before.0 + 99)
    );
    assert_eq!(
        workbench
            .sidebar
            .threads
            .iter()
            .find(|t| t.id.to_string() == "lane-thread-2")
            .unwrap()
            .updated_at,
        before.1
    );
}

#[test]
fn polling_alone_never_moves_a_lane() {
    let ledger = Arc::new(FakeLedger::default());
    ledger.put(snapshot(
        record(1, Some("thread"), LedgerPolicy::Attached, 1),
        None,
    ));
    let (mut workbench, _dir) = workbench(Some(ledger));
    settle(&mut workbench);
    settle(&mut workbench);
    let settled = workbench.sidebar.clone();

    for _ in 0..5 {
        assert!(!workbench.pump_lanes());
    }

    assert_eq!(workbench.sidebar, settled);
}

#[test]
fn the_database_reader_finds_lanes_the_sidebar_lost() {
    use storage::lanes::NewLane;

    // Given: a stored lane (as the runtime creates it) under the sidebar's project root
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let root = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let brief = record(1, Some("thread"), LedgerPolicy::Attached, 1).brief;
    let created = storage
        .handle()
        .create_lane(NewLane {
            lane_id: "lane-1".into(),
            thread_id: "lane-thread-1".into(),
            origin_thread_id: "thread".into(),
            origin_run_id: "lead-run".into(),
            creation_key: "call-1".into(),
            brief,
            start_seed: "seed".into(),
            project_id: Some("test".into()),
            project_root: Some(root),
            title: Some("Stored lane".into()),
            at_ns: 5_000_000_000,
        })
        .unwrap();
    assert!(created.created);

    // When: a Workbench with no record of it starts with the database reader
    let reader = Arc::new(super::lanes::StorageLaneReader::new(config.db_path.clone()));
    let mut workbench = state(&dir.path().join("sidebar.json"))
        .with_lane_ledger(reader)
        .with_lane_spawner(inline_spawner());
    settle(&mut workbench);

    // Then: the lane is repaired into the sidebar under its lead, with its stored title
    let lane = workbench
        .sidebar
        .threads
        .iter()
        .find(|thread| thread.id.to_string() == "lane-thread-1")
        .expect("the lane must be repaired");
    assert_eq!(lane.title, "Stored lane");
    assert_eq!(family(&workbench, "lane-thread-1"), "thread");
    assert_eq!(ids(&workbench, "lane-thread-1"), 1);
}

/// Inline reads finish inside the poll that starts them and are drained by the next one.
fn settle(workbench: &mut WorkbenchState<DemoSource>) {
    for _ in 0..3 {
        workbench.pump_lanes();
    }
}

/// The retention input the GUI builds and the storage closure agree on the unit of
/// deletion: a lead and the lanes it manages, but not a detached lane.
#[test]
fn retention_deletes_a_lead_with_its_managed_lanes_and_never_with_a_detached_one() {
    use storage::lanes::NewLane;
    use storage::thread_retention::{
        ExploreBudget, FamilyState, RetentionInput, ThreadRetentionPolicy,
    };
    use storage::threads::ThreadUpsert;

    // Given: a stored lead, an attached lane and a detached lane, as the runtime writes them
    let dir = tempfile::tempdir().unwrap();
    let config = storage::StorageConfig {
        db_path: dir.path().join("events.db"),
        ..Default::default()
    };
    let storage = storage::Storage::open(config.clone()).unwrap();
    let handle = storage.handle();
    let root = std::fs::canonicalize(dir.path())
        .unwrap()
        .to_string_lossy()
        .into_owned();
    handle
        .upsert_threads_host(&[ThreadUpsert {
            thread_id: "thread".into(),
            project_root: Some(root.clone()),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        }])
        .unwrap();
    let mut brief = record(1, Some("thread"), LedgerPolicy::Attached, 1).brief;
    for (n, policy) in [(1, LedgerPolicy::Attached), (2, LedgerPolicy::Detached)] {
        brief.report_policy = policy;
        handle
            .create_lane(NewLane {
                lane_id: format!("lane-{n}"),
                thread_id: format!("lane-thread-{n}"),
                origin_thread_id: "thread".into(),
                origin_run_id: "lead-run".into(),
                creation_key: format!("call-{n}"),
                brief: brief.clone(),
                start_seed: "seed".into(),
                project_id: Some("test".into()),
                project_root: Some(root.clone()),
                title: None,
                at_ns: 1,
            })
            .unwrap();
    }

    // And: the sidebar as the events build it, with the lead's family archived
    let mut workbench = state(&dir.path().join("sidebar.json"));
    for (n, lead, policy) in [
        (1, Some("thread"), LedgerPolicy::Attached),
        (2, None, LedgerPolicy::Detached),
    ] {
        workbench.apply_conversation_event(&created(&record(n, lead, policy, 1)));
    }
    workbench
        .sidebar
        .set_family_archived(&ThreadId::new("thread"), true, 1)
        .unwrap();
    let input = RetentionInput {
        threads: crate::startup::sidebar_threads(&workbench.sidebar),
        sidebar_path: "/sidebar.json".into(),
        ownership_root: "/root".into(),
        now_ns: i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        )
        .unwrap()
            + 400 * 86_400 * 1_000_000_000,
    };

    // When: retention explores the archived families on the real database
    let database = storage::Database::open(&config).unwrap();
    let states = database
        .retention_explore(
            &input,
            &ThreadRetentionPolicy::default(),
            ExploreBudget { max_edges: 100_000 },
        )
        .unwrap();

    // Then: the lead and its managed lane are one family to delete; the detached lane,
    // which the sidebar keeps active, is not part of it
    let families: Vec<Vec<String>> = states
        .iter()
        .filter_map(|(_, state)| match state {
            FamilyState::Explored { threads, .. } => Some(threads.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        families,
        [["lane-thread-1", "thread"].map(String::from).to_vec()],
        "{states:?}"
    );
}

#[test]
fn an_event_that_arrives_while_a_read_is_in_flight_triggers_one_more_read() {
    // Given: a spawner that holds the jobs, and a lane whose first read sees no report
    let ledger = Arc::new(FakeLedger::default());
    let lane = record(1, Some("thread"), LedgerPolicy::Attached, 1);
    ledger.put(snapshot(lane.clone(), None));
    type Job = Box<dyn FnOnce() + Send>;
    let held: Arc<Mutex<Vec<Job>>> = Arc::default();
    let spawner: Spawner = {
        let held = Arc::clone(&held);
        Arc::new(move |job| held.lock().unwrap().push(job))
    };
    let (workbench, _dir) = workbench(None);
    let mut workbench = workbench
        .with_lane_ledger(Arc::clone(&ledger) as Arc<dyn LaneLedgerReader>)
        .with_lane_spawner(spawner);
    let run_held = || {
        let jobs: Vec<_> = held.lock().unwrap().drain(..).collect();
        for job in jobs {
            job();
        }
    };
    workbench.apply_conversation_event(&created(&lane));
    workbench.pump_lanes();
    run_held();

    // When: the read has finished but is not collected yet, a Report is appended and its
    // event is folded
    let mut updated = lane.clone();
    updated.latest_report_seq = 1;
    ledger.put(snapshot(updated, Some(report(1, 1, LaneStatus::Ready))));
    workbench.apply_conversation_event(&Event::new(LedgerEvent::LaneReportAppended {
        lane_id: "lane-1".into(),
        thread_id: "lane-thread-1".into(),
        run_id: None,
        report_seq: 1,
        status: LaneStatus::Ready,
        needs_human: false,
    }));
    workbench.pump_lanes();
    assert!(
        workbench.lanes.details["lane-thread-1"]
            .latest_report
            .is_none()
    );
    run_held();
    workbench.pump_lanes();

    // Then: the stale result is replaced by a second read, not kept until the next event
    assert_eq!(
        workbench.lanes.details["lane-thread-1"].status(),
        Some(LaneStatus::Ready)
    );
}

#[derive(Clone, Default)]
struct RootBindings(Arc<Mutex<Vec<(String, String)>>>);

impl RootBindings {
    fn latest(&self, thread: &str) -> Option<String> {
        self.0
            .lock()
            .unwrap()
            .iter()
            .rev()
            .find(|(bound, _)| bound == thread)
            .map(|(_, run)| run.clone())
    }
}

struct BindingSink(RootBindings);

impl crate::model::commands::CommandSink for BindingSink {
    fn submit(
        &mut self,
        _: crate::model::commands::WorkbenchCommand,
    ) -> Vec<crate::model::commands::LoopEvent> {
        Vec::new()
    }

    fn bind_lane_root(&mut self, thread: &str, run: &str) {
        self.0
            .0
            .lock()
            .unwrap()
            .push((thread.to_owned(), run.to_owned()));
    }
}

fn root_started(run: &str, thread: &str) -> Event {
    Event::new(event_bus::LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: None,
        agent_name: format!("chat:Orchestrator:{thread}"),
        role: "Orchestrator".into(),
    })
}

#[test]
fn the_command_sink_follows_the_lane_root_whichever_event_arrives_first_and_after_a_new_root() {
    // Given: a workbench whose sink records the lane roots it is given
    let bindings = RootBindings::default();
    let (workbench, _dir) = workbench(None);
    let mut workbench = workbench.with_command_sink(Box::new(BindingSink(bindings.clone())));

    // When: the runtime-started root announces itself before the lane's creation event
    workbench.apply_conversation_event(&root_started("lane-root-a", "lane-thread-1"));
    assert_eq!(bindings.latest("lane-thread-1"), None);
    workbench.apply_conversation_event(&created(&record(
        1,
        Some("thread"),
        LedgerPolicy::Attached,
        1,
    )));

    // Then: the creation event places the thread and hands the sink its root
    assert_eq!(
        bindings.latest("lane-thread-1").as_deref(),
        Some("lane-root-a")
    );

    // When: the lane gets another root
    workbench.apply_conversation_event(&root_started("lane-root-b", "lane-thread-1"));

    // Then: the sink points at the newest root
    assert_eq!(
        bindings.latest("lane-thread-1").as_deref(),
        Some("lane-root-b")
    );
}

#[test]
fn a_lane_restored_from_the_ledger_gives_the_sink_its_recorded_root_and_a_plain_chat_gives_none() {
    // Given: a ledger lane the sidebar never heard of, and a sink that records roots
    let ledger = Arc::new(FakeLedger::default());
    ledger.put(snapshot(
        record(1, Some("thread"), LedgerPolicy::Attached, 1),
        None,
    ));
    let bindings = RootBindings::default();
    let (workbench, _dir) = workbench(Some(Arc::clone(&ledger)));
    let mut workbench = workbench.with_command_sink(Box::new(BindingSink(bindings.clone())));

    // When: the startup scan places it, and an ordinary chat root starts
    settle(&mut workbench);
    workbench.apply_conversation_event(&root_started("chat-root", "thread"));

    // Then: only the lane's thread was bound, to the root the ledger records
    assert_eq!(
        bindings.latest("lane-thread-1").as_deref(),
        Some("lane-root-1")
    );
    assert_eq!(bindings.latest("thread"), None);
}
