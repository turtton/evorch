//! Auto-archive fixtures own synthetic DB, sidebar and attachments. No wall-clock verdicts:
//! every tick receives its `now` and every event its time.
mod common;
use std::sync::{Arc, Mutex};
use storage::WriterLeases;
use storage::thread_retention::ExclusiveFixtureStorage;
use storage::thread_retention::{AutoArchiveAuthority, AutoArchivePolicy};
use storage::{Database, StorageConfig, StorageHandle};
use workbench_host::thread_retention::{AutoArchiveCoordinator, RetentionReport};
use workbench_host::{
    CommandHost,
    areas::{Areas, workspace::WorkspaceArea},
    commands::CommandService,
};
use workspace_state::{ProjectId, SidebarDocument, SidebarStore, ThreadId, ThreadRecord};
const DAY: i64 = 86_400_000_000_000;

struct World {
    dir: tempfile::TempDir,
    config: StorageConfig,
    store: SidebarStore,
    rt: tokio::runtime::Runtime,
    scratch_hints: Arc<Mutex<Vec<String>>>,
}
impl World {
    fn new(runs: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("events.db"),
            ..Default::default()
        };
        Database::open(&config).unwrap();
        let conn = rusqlite::Connection::open(&config.db_path).unwrap();
        for n in 1..=runs {
            conn.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,'thread','run-1',1)",[format!("run-{n}")]).unwrap();
            conn.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Message','MessageCompleted',?1,0,'{}')",[format!("run-{n}")]).unwrap();
        }
        let store = SidebarStore::new(dir.path().join("sidebar.json"));
        let mut doc = SidebarDocument::default();
        let project = ProjectId::new("project");
        doc.sidebar
            .add_project(project.clone(), "project", dir.path())
            .unwrap();
        let mut record = ThreadRecord::new(ThreadId::new("thread"), project, "thread");
        record.created_at = 0;
        record.updated_at = 0;
        record.root_run_id = Some("run-1".into());
        doc.sidebar.threads.push(record);
        doc.sidebar.refresh_derived();
        store.write(&store.lock().unwrap(), &doc).unwrap();
        Self {
            dir,
            config,
            store,
            scratch_hints: Default::default(),
            rt: tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap(),
        }
    }
    fn leases(&self) -> WriterLeases {
        WriterLeases::acquire(
            &self.config.db_path,
            self.dir.path(),
            std::time::Duration::ZERO,
        )
        .unwrap()
    }
    fn host(&self, handle: StorageHandle) -> Arc<Mutex<CommandHost>> {
        let runtime = common::runtime()
            .with_run_store(runtime::RunStore::open(&self.config, handle.clone()).unwrap());
        let attachments =
            sandbox::attachments::AttachmentRoot::at(self.dir.path().join("attachments"));
        runtime.set_thread_scratch(Arc::new(
            runtime::scratch::ThreadScratch::at(self.dir.path().join("scratch"))
                .with_fixture_attachments(attachments),
        ));
        let area = WorkspaceArea::with_store(
            SidebarStore::new(self.store.path()),
            Some(runtime.clone()),
            vec![],
            None,
            |_| false,
        )
        .with_mirror(handle)
        .with_scratch_hint({
            let seen = self.scratch_hints.clone();
            Arc::new(move |id| seen.lock().unwrap().push(id.into()))
        });
        Arc::new(Mutex::new(
            CommandHost::new(CommandService::new(runtime, self.rt.handle().clone())).with_areas(
                Areas {
                    workspace: area,
                    ..Default::default()
                },
            ),
        ))
    }
    fn coordinator(&self) -> AutoArchiveCoordinator {
        AutoArchiveCoordinator::new(
            self.config.clone(),
            self.dir.path().into(),
            AutoArchivePolicy::default(),
        )
    }
    fn archived(&self) -> bool {
        self.store.read_unlocked().unwrap().sidebar.threads[0].archived
    }
    /// Ticks until one publishes or holds for a reason other than finishing a scan.
    fn drive(
        &self,
        c: &mut AutoArchiveCoordinator,
        h: &Arc<Mutex<CommandHost>>,
        a: &AutoArchiveAuthority<'_>,
        now: i64,
    ) -> RetentionReport {
        for _ in 0..5 {
            let r = c.tick(h, Some(a), now);
            assert!(r.published <= 1);
            assert!(r.attachment_calls <= 1);
            assert!(r.scratch_hints <= 1);
            assert!(r.failures.is_empty(), "{:?}", r.failures);
            if r.published > 0 || r.holds.keys().any(|k| *k != "ScanComplete") {
                return r;
            }
        }
        panic!("no family was ever decided");
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.config.db_path).unwrap()
    }
    fn runtime(&self, host: &Arc<Mutex<CommandHost>>) -> runtime::AgentRuntime {
        host.lock().unwrap().service().runtime().clone()
    }
}
fn goal_event(
    thread: &str,
    root: &str,
    goal: &str,
    phase: event_bus::ThreadGoalPhase,
) -> event_bus::EventKind {
    event_bus::OrchestratorEvent::ThreadGoalUpdated {
        snapshot: event_bus::ThreadGoalSnapshot {
            goal_id: goal.into(),
            thread_id: thread.into(),
            root_run_id: root.into(),
            related_root_run_ids: vec![],
            objective: "objective".into(),
            criteria: vec![],
            checks: vec![],
            phase,
            review_enabled: false,
            checks_paused: false,
            work_stopped: false,
            epoch: 0,
            review_round: 0,
            findings: vec![],
            reason: None,
            usage: Default::default(),
            max_review_rounds: 1,
            max_tokens: None,
            original_request: "request".into(),
        },
    }
    .into()
}
fn put_goal(sql: &rusqlite::Connection, event: &event_bus::EventKind, at: i64) {
    sql.execute(
        "INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated',?1,?2,?3)",
        rusqlite::params![event.primary_run_id(), at, serde_json::to_string(event).unwrap()],
    )
    .unwrap();
}
macro_rules! live {
    ($w:ident, $leases:ident, $storage:ident, $host:ident, $a:ident, $c:ident) => {
        let $leases = $w.leases();
        let $storage =
            ExclusiveFixtureStorage::open($w.config.clone(), $w.dir.path(), &$leases).unwrap();
        let $host = $w.host($storage.handle());
        let $a = $leases.authority().unwrap();
        let mut $c = $w.coordinator();
    };
}
#[test]
fn no_authority_touches_nothing_even_with_every_lock_held_and_no_database() {
    let w = World::new(1);
    let host = w.host(storage::Storage::open(w.config.clone()).unwrap().handle());
    // The database named here cannot be opened: a tick that read it would fail.
    let missing = StorageConfig {
        db_path: w.dir.path().join("absent").join("events.db"),
        ..Default::default()
    };
    let mut c =
        AutoArchiveCoordinator::new(missing, w.dir.path().into(), AutoArchivePolicy::default());
    let before = std::fs::read(w.store.path()).unwrap();
    let sidebar = w.store.lock().unwrap();
    let attachment = sandbox::attachments::AttachmentRoot::at(w.dir.path().join("attachments"))
        .lock_thread("thread")
        .unwrap();
    let foreground = host.lock().unwrap();
    for _ in 0..3 {
        let r = c.tick(&host, None, 8 * DAY);
        assert_eq!(r.holds.len(), 1);
        assert_eq!(r.holds.get("AuthorityUnavailable"), Some(&1));
        assert!(r.failures.is_empty(), "{:?}", r.failures);
        assert_eq!(r.decision, None);
        assert_eq!(
            (r.published, r.attachment_calls, r.scratch_hints),
            (0, 0, 0)
        );
        assert_eq!(r.inspected_runs, 0);
    }
    assert_eq!(std::fs::read(w.store.path()).unwrap(), before);
    drop((foreground, attachment, sidebar));
    assert!(!w.archived());
}
#[test]
fn due_family_publishes_once_and_keeps_real_commit_time_on_retry() {
    let w = World::new(3);
    live!(w, leases, storage, host, a, c);
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 7 * DAY - 1);
    assert_eq!((r.published, r.holds.get("NotDue")), (0, Some(&1)));
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 7 * DAY);
    assert_eq!(r.published, 1);
    assert!(r.decision.unwrap().statements > 0);
    assert!(w.archived());
    let before = w.store.read_unlocked().unwrap().sidebar.threads[0].archived_at;
    c.tick(&host, Some(&a), 9 * DAY);
    assert_eq!(
        w.store.read_unlocked().unwrap().sidebar.threads[0].archived_at,
        before
    );
    assert_eq!(before, Some(7 * 86_400));
}
#[test]
fn a_large_family_is_decided_in_one_tick_and_its_newest_run_rules_it() {
    let w = World::new(624);
    live!(w, leases, storage, host, a, c);
    w.sql()
        .execute(
            "UPDATE events SET wall_clock_ns=?1 WHERE run_id='run-624'",
            [6 * DAY],
        )
        .unwrap();
    let r = c.tick(&host, Some(&a), 8 * DAY);
    assert_eq!(r.holds.get("RecentActivity"), Some(&1), "{r:?}");
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 13 * DAY);
    assert_eq!(r.published, 1, "{r:?}");
    assert!(r.decision.unwrap().tail_rows <= 512);
    assert!(w.archived());
}
#[test]
fn a_family_beyond_the_run_budget_is_never_archived_automatically() {
    let w = World::new(1_100);
    live!(w, leases, storage, host, a, c);
    let r = w.drive(&mut c, &host, &a, 99 * DAY);
    assert_eq!(r.published, 0);
    assert_eq!(r.holds.get("FamilyRunBudget"), Some(&1), "{r:?}");
    assert!(!w.archived());
}
#[test]
fn held_foreground_mutex_defers_background_without_changing_foreground_contract() {
    let w = World::new(1);
    live!(w, leases, storage, host, a, c);
    let foreground = host.lock().unwrap();
    assert_eq!(
        c.tick(&host, Some(&a), 8 * DAY)
            .holds
            .get("ForegroundPending"),
        Some(&1)
    );
    assert!(!w.archived());
    drop(foreground);
    assert_eq!(w.drive(&mut c, &host, &a, 8 * DAY).published, 1);
}
#[test]
fn an_admission_after_the_stamps_cancels_the_publication_and_a_pending_one_defers_it() {
    let w = World::new(1);
    live!(w, leases, storage, host, a, c);
    let runtime = w.runtime(&host);
    let bus = runtime.event_bus();
    c.on_decision(move || {
        bus.emit(event_bus::Event::new(
            event_bus::MessageEvent::MessageCompleted {
                run_id: "run-1".into(),
                text: "arrived while deciding".into(),
            },
        ));
    });
    let r = c.tick(&host, Some(&a), 8 * DAY);
    assert_eq!(
        (r.published, r.holds.get("StaleRevision")),
        (0, Some(&1)),
        "{r:?}"
    );
    assert!(!w.archived());
    // the event is still waiting for its persistence acknowledgement
    let r = w.drive(&mut c, &host, &a, 8 * DAY);
    assert_eq!(r.holds.get("ForegroundPending"), Some(&1), "{r:?}");
    assert!(!w.archived());
}
#[test]
fn a_change_between_ticks_is_seen_by_the_next_tick_because_no_verdict_is_carried() {
    let w = World::new(1);
    live!(w, leases, storage, host, a, c);
    let sql = w.sql();
    sql.execute_batch("INSERT INTO thread_start_states(thread_id,root_run_id,project_root,repo_prefix,head_commit,diff,captured_at_ns,replayable) VALUES('thread','run-1','/fixture','','fixture',X'',0,1); INSERT INTO eval_samples(thread_id,problem,focus_roles,created_at_ns,updated_at_ns) VALUES('thread','p','[]',0,0);").unwrap();
    assert!(
        w.drive(&mut c, &host, &a, 8 * DAY)
            .holds
            .contains_key("EvaluationSample")
    );
    // another connection removes the protection and rewrites an old row to a recent header
    sql.execute("DELETE FROM eval_samples", []).unwrap();
    sql.execute(
        "UPDATE events SET wall_clock_ns=?1 WHERE run_id='run-1'",
        [8 * DAY],
    )
    .unwrap();
    let r = w.drive(&mut c, &host, &a, 8 * DAY);
    assert_eq!(
        (r.published, r.holds.get("RecentActivity")),
        (0, Some(&1)),
        "{r:?}"
    );
    sql.execute("UPDATE events SET wall_clock_ns=0 WHERE run_id='run-1'", [])
        .unwrap();
    assert_eq!(w.drive(&mut c, &host, &a, 8 * DAY).published, 1);
}
#[test]
fn a_goal_handed_to_another_family_releases_its_source_but_an_uncommitted_receipt_does_not() {
    let w = World::new(1);
    live!(w, leases, storage, host, a, c);
    let sql = w.sql();
    put_goal(
        &sql,
        &goal_event("thread", "run-1", "g1", event_bus::ThreadGoalPhase::Working),
        0,
    );
    let r = w.drive(&mut c, &host, &a, 8 * DAY);
    assert_eq!(r.holds.get("UnfinishedGoal"), Some(&1), "{r:?}");
    sql.execute("INSERT INTO lane_handoffs(source_run_id,source_incarnation,target_run_id,lane_id,request_json,state,goal_json) VALUES('run-1','i','target','lane','{}','prepared','{\"goal_id\":\"g1\"}')", []).unwrap();
    let r = w.drive(&mut c, &host, &a, 8 * DAY);
    assert_eq!(r.holds.get("UnfinishedGoal"), Some(&1), "{r:?}");
    sql.execute("UPDATE lane_handoffs SET state='committed'", [])
        .unwrap();
    assert_eq!(w.drive(&mut c, &host, &a, 8 * DAY).published, 1);
    assert!(w.archived());
}
// One protected descendant blocks the whole family, not just its own record.
#[test]
fn canonical_descendant_protections_hold_the_family() {
    for (case, reason) in [
        ("pin", "Pinned"),
        ("goal", "UnfinishedGoal"),
        ("unreadable-goal", "GoalUndecodable"),
        ("sample", "EvaluationSample"),
        ("question", "UnansweredQuestion"),
        ("inherited", "UnansweredQuestion"),
        ("attached", "OpenManagedLane"),
        ("summary", "OpenManagedLane"),
    ] {
        let w = World::new(1);
        let lock = w.store.lock().unwrap();
        let mut doc = w.store.read(&lock).unwrap();
        let mut child =
            ThreadRecord::new(ThreadId::new("child"), ProjectId::new("project"), "child");
        child.created_at = 0;
        child.updated_at = 0;
        child.parent_thread_id = Some(ThreadId::new("thread"));
        child.root_run_id = Some("run-2".into());
        doc.sidebar.threads.push(child);
        doc.sidebar.refresh_derived();
        if case == "pin" {
            doc.sidebar
                .set_family_pinned(&ThreadId::new("child"), true)
                .unwrap();
        }
        w.store.write(&lock, &doc).unwrap();
        drop(lock);
        live!(w, leases, storage, host, a, c);
        let sql = w.sql();
        sql.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES('run-2','child','run-2',0)",[]).unwrap();
        sql.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Message','MessageCompleted','run-2',0,'{}')",[]).unwrap();
        match case {
            "goal" => put_goal(
                &sql,
                &goal_event("child", "run-2", "g", event_bus::ThreadGoalPhase::Working),
                0,
            ),
            "unreadable-goal" => {
                sql.execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) VALUES(1,0,'Orchestrator','ThreadGoalUpdated','run-2',0,'{\"broken\"')", []).unwrap();
            }
            "sample" => {
                sql.execute_batch("INSERT INTO thread_start_states(thread_id,root_run_id,project_root,repo_prefix,head_commit,diff,captured_at_ns,replayable) VALUES('child','run-2','/fixture','','fixture',X'',0,1); INSERT INTO eval_samples(thread_id,problem,focus_roles,created_at_ns,updated_at_ns) VALUES('child','problem','[]',0,0);").unwrap();
            }
            "question" => {
                sql.execute("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q','run-3','run-2','root','{}')",[]).unwrap();
            }
            "inherited" => {
                sql.execute_batch("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload) VALUES('q','run-4','outside','root','{}'); INSERT INTO user_question_links(question_id,run_id) VALUES('q','run-2');").unwrap();
            }
            "attached" | "summary" => {
                let brief=serde_json::json!({"objective":"objective","inputs":[],"deliverables":[],"acceptance_criteria":[],"constraints":[],"report_policy":case}).to_string();
                sql.execute("INSERT INTO lanes(lane_id,thread_id,origin_thread_id,origin_run_id,creation_key,request_hash,scope,lead_thread_id,policy,management_revision,brief_json,brief_revision,start_seed,root_run_id,generation,created_at_ns,updated_at_ns,last_activity_at_ns) VALUES('lane','child','thread','run-1','creation','hash','lane','thread',?1,1,?2,1,'{}','run-2',1,0,0,0)",rusqlite::params![case,brief]).unwrap();
            }
            _ => {}
        }
        let r = w.drive(&mut c, &host, &a, 8 * DAY);
        assert_eq!(r.published, 0, "{case}");
        assert!(r.holds.contains_key(reason), "{case}: {:?}", r.holds);
        assert!(!w.archived(), "{case}");
    }
}
#[test]
fn capability_cannot_authorize_another_mirror_writer() {
    let w = World::new(1);
    let other = World::new(1);
    let leases = w.leases();
    let _storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    let wrong = storage::Storage::open(other.config.clone()).unwrap();
    let host = w.host(wrong.handle());
    let before = std::fs::read(w.store.path()).unwrap();
    let report = w
        .coordinator()
        .tick(&host, Some(&leases.authority().unwrap()), 8 * DAY);
    assert_eq!(report.published, 0);
    assert_eq!(report.attachment_calls, 0);
    assert!(
        report.holds.contains_key("AuthorityUnavailable"),
        "{report:?}"
    );
    assert_eq!(std::fs::read(w.store.path()).unwrap(), before);
}

fn many_attachments(w: &World) -> (sandbox::attachments::AttachmentRoot, std::path::PathBuf) {
    let root = sandbox::attachments::AttachmentRoot::at(w.dir.path().join("attachments"));
    let files = root.files("thread").unwrap();
    for i in 0..4098 {
        std::fs::write(files.path().join(format!("{i}.bin")), b"fixture").unwrap();
    }
    (root, files.path().to_owned())
}

#[test]
fn partial_attachment_cleanup_resumes_after_restart_with_archiving_disabled_and_does_not_follow_symlinks()
 {
    let w = World::new(1);
    let (_root, files) = many_attachments(&w);
    let outside = w.dir.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    std::fs::write(outside.join("keep"), b"keep").unwrap();
    std::os::unix::fs::symlink(&outside, files.join("outside-link")).unwrap();
    let leases = w.leases();
    let authority = leases.authority().unwrap();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    let host = w.host(storage.handle());
    let mut first = w.coordinator();
    let applied = w.drive(&mut first, &host, &authority, 7 * DAY);
    assert_eq!(applied.published, 1);
    assert_eq!(applied.attachment_calls, 1);
    assert!(applied.holds.contains_key("AttachmentCleanupPending"));
    assert!(files.exists());
    let remaining = std::fs::read_dir(&files).unwrap().count();
    assert!((1..4098).contains(&remaining));
    let before =
        w.store.read_unlocked().unwrap().sidebar.resolve_states()[&ThreadId::new("thread")].clone();
    let mut restarted = AutoArchiveCoordinator::new(
        w.config.clone(),
        w.dir.path().into(),
        AutoArchivePolicy { after_days: 0 },
    );
    let resumed = restarted.tick(&host, Some(&authority), 8 * DAY);
    assert_eq!(resumed.published, 0);
    assert_eq!(resumed.attachment_calls, 1);
    assert_eq!(resumed.scratch_hints, 1);
    assert_eq!(*w.scratch_hints.lock().unwrap(), ["thread", "thread"]);
    assert!(!files.exists());
    assert!(outside.join("keep").is_file());
    let after =
        w.store.read_unlocked().unwrap().sidebar.resolve_states()[&ThreadId::new("thread")].clone();
    assert_eq!(before.archive_revision, after.archive_revision);
    assert_eq!(before.archived_at, after.archived_at);
}

#[test]
fn manual_restore_finishes_old_cleanup_and_stale_cursor_keeps_new_active_attachments() {
    use workbench_protocol::{
        commands::CommandResult,
        namespaces::{WorkspaceQuery, WorkspaceReply, workspace::*},
        queries::PageRequest,
    };
    let w = World::new(1);
    let (root, files) = many_attachments(&w);
    let leases = w.leases();
    let authority = leases.authority().unwrap();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    let host = w.host(storage.handle());
    let mut c = w.coordinator();
    assert_eq!(w.drive(&mut c, &host, &authority, 7 * DAY).published, 1);
    assert!(files.exists());
    {
        // Exercise the real synchronous manual wrapper under the host mutex,
        // including attachment-before-sidebar cleanup and mirror/notifications.
        let foreground = host.lock().unwrap();
        let area = WorkspaceArea::with_store(
            w.store.clone(),
            Some(foreground.service().runtime().clone()),
            vec![],
            None,
            |_| false,
        )
        .with_mirror(storage.handle());
        let WorkspaceReply::Families(page) = area
            .query(&WorkspaceQuery::ListFamilies(PageRequest::default()))
            .unwrap()
        else {
            panic!("families")
        };
        let family = &page.items[0];
        let reply = area
            .command(
                WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                    thread: family.key.clone(),
                    archived: false,
                    expected_family: FamilyIdentity {
                        key: family.key.clone(),
                        membership_epoch: family.membership_epoch,
                    },
                    expected_revision: family.archive_revision,
                }),
                &workbench_host::provenance::local_desktop(),
            )
            .unwrap();
        assert!(matches!(
            *reply.result.unwrap(),
            CommandResult::Workspace(WorkspaceResult::Applied(_))
        ));
    }
    assert!(!files.exists());
    let fresh = root.files("thread").unwrap().path().join("fresh");
    std::fs::write(&fresh, b"new active attachment").unwrap();
    // A legitimate new accepted message, not the restore itself, advances
    // activity. All clocks are injected; Event::new's wall clock is not used.
    rusqlite::Connection::open(&w.config.db_path).unwrap().execute("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload)VALUES(1,0,'Message','MessageCompleted','run-1',?1,'{}')",[8*DAY]).unwrap();
    let report = c.tick(&host, Some(&authority), 8 * DAY);
    assert_eq!(report.published, 0);
    assert_eq!(report.attachment_calls, 0);
    assert!(fresh.is_file());
    assert!(
        !w.store.read_unlocked().unwrap().sidebar.resolve_states()[&ThreadId::new("thread")]
            .archived
    );
}

#[cfg(test)]
mod c0_measure {
    // Manual final C-0: ticks of the real coordinator on a synthetic sidebar. Measurement only; no assertion depends on time.
    use super::*;
    use std::time::Instant;
    use workspace_state::LineageKind;
    fn run_id(i: usize, r: usize) -> String {
        format!("run-{}", i * 1000 + r + 1)
    }
    fn summary(v: &[f64]) -> serde_json::Value {
        let mut sorted = v.to_vec();
        sorted.sort_by(f64::total_cmp);
        serde_json::json!({"n":v.len(),"p95_ms":sorted[(v.len()*95).div_ceil(100)-1],"max_ms":sorted[sorted.len()-1]})
    }
    fn measure(count: usize) {
        let output = std::env::var("RETENTION_MEASURE_DIR").unwrap();
        let execution = std::env::var("C0_RUN").unwrap();
        let w = World::new(0);
        let config = w.config.clone();
        let temp = &w.dir;
        let leases = w.leases();
        let storage = ExclusiveFixtureStorage::open(config.clone(), w.dir.path(), &leases).unwrap();
        let mut sql = rusqlite::Connection::open(&config.db_path).unwrap();
        let tx = sql.transaction().unwrap();
        let mut runs = 0;
        for i in 0..count {
            for r in 0..if i == 0 { 624 } else { 5 } {
                let run = run_id(i, r);
                tx.execute("INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,?2,?3,?4)",
                    rusqlite::params![run,format!("thread-{i:06}"),run_id(i, 0),i64::from(r!=0)]).unwrap();
                tx.execute("INSERT INTO thread_index_runs(run_id,state,source_revision,snapshot_revision,source_context_sha256,extractor_version,accounting_version,source_updated_at_ns,retention_at_ns) VALUES(?1,'pending',1,1,'synthetic',1,1,1700000000000000000,1700000000000000000)", [&run]).unwrap();
                tx.execute(
                    "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,?2,?2)",
                    rusqlite::params![
                        format!("run-{}", 1_000_000_001usize + i * 1000 + r),
                        format!("deleted-thread-{i:06}")
                    ],
                )
                .unwrap();
                runs += 1;
            }
            tx.execute("INSERT INTO thread_tombstones(thread_id,family_key,deleted_at_ns) VALUES(?1,?1,1700000000000000000)", [format!("deleted-thread-{i:06}")]).unwrap();
        }
        tx.commit().unwrap();
        assert_eq!(
            sql.query_row("SELECT count(*) FROM thread_index_runs", [], |row| row
                .get::<_, usize>(0))
                .unwrap(),
            runs
        );
        let store = SidebarStore::new(temp.path().join("sidebar.json"));
        let mut document = SidebarDocument::default();
        for p in 0..8 {
            let root = temp.path().join(format!("project-{p}"));
            std::fs::create_dir(&root).unwrap();
            document
                .sidebar
                .add_project(
                    workspace_state::ProjectId::new(format!("project-{p}")),
                    format!("Project {p}"),
                    &root,
                )
                .unwrap();
        }
        for i in 0..count {
            let project = workspace_state::ProjectId::new(format!("project-{}", (i / 4) % 8));
            let mut thread = ThreadRecord::new_at(
                ThreadId::new(format!("thread-{i:06}")),
                project,
                format!("Synthetic conversation {i}"),
                std::time::UNIX_EPOCH,
            );
            thread.created_at = 1_700_000_000;
            thread.updated_at = 1_700_100_000;
            thread.draft_input = "d".repeat(128);
            thread.run_ids = (0..if i == 0 { 624 } else { 5 })
                .map(|r| run_id(i, r))
                .collect();
            thread.root_run_id = thread.run_ids.first().cloned();
            if i % 4 != 0 {
                thread.parent_thread_id = Some(ThreadId::new(format!("thread-{:06}", i - i % 4)));
                thread.lineage = Some(workspace_state::ThreadLineage {
                    kind: LineageKind::Fork,
                    point: None,
                });
            }
            document.sidebar.threads.push(thread);
        }
        let mut lanes = 0;
        for i in (5..count).step_by(10) {
            let lead = i - i % 4;
            let p = (i / 4) % 8;
            storage
                .handle()
                .create_lane(storage::lanes::NewLane {
                    lane_id: format!("lane-{i:06}"),
                    thread_id: format!("thread-{i:06}"),
                    origin_thread_id: format!("thread-{lead:06}"),
                    origin_run_id: run_id(lead, 0),
                    creation_key: format!("create-{i}"),
                    brief: storage::lanes::LaneBrief {
                        verification_requirements: Default::default(),
                        objective: "synthetic objective".into(),
                        inputs: vec![],
                        deliverables: vec!["synthetic result".into()],
                        acceptance_criteria: vec!["synthetic checks".into()],
                        constraints: vec![],
                        workspace: Default::default(),
                        report_policy: storage::lanes::LanePolicy::Attached,
                    },
                    start_seed: "synthetic seed".into(),
                    project_id: Some(format!("project-{p}")),
                    project_root: Some(
                        temp.path()
                            .join(format!("project-{p}"))
                            .to_string_lossy()
                            .into_owned(),
                    ),
                    title: Some(format!("Lane {i}")),
                    at_ns: 1700000000000000000,
                })
                .unwrap();
            lanes += 1;
        }
        assert_eq!(lanes, count / 10);
        document.sidebar.refresh_derived();
        let lock = store.lock().unwrap();
        store.write(&lock, &document).unwrap();
        drop(lock);

        sql.execute_batch("INSERT INTO events(schema_version,monotonic_ns,kind,subkind,run_id,wall_clock_ns,payload) SELECT 1,0,'Message','MessageCompleted',run_id,1700100000000000000,'{}' FROM thread_runs").unwrap();
        drop(sql);
        let host = w.host(storage.handle());
        let authority = leases.authority().unwrap();
        let now = 1_702_000_000_000_000_000;
        let (mut heavy, mut normal_published, mut normal_held) =
            (Vec::new(), Vec::new(), Vec::new());
        let mut heavy_decision = None;
        for trial in 0..32 {
            store.write(&store.lock().unwrap(), &document).unwrap();
            let mut coordinator = w.coordinator();
            for tick in 0..41 {
                let started = Instant::now();
                let report = coordinator.tick(&host, Some(&authority), now);
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                assert!(report.failures.is_empty(), "{:?}", report.failures);
                assert!(
                    report.published <= 1
                        && report.attachment_calls <= 1
                        && report.scratch_hints <= 1
                );
                if tick == 0 {
                    assert_eq!(report.published, 1, "{report:?}");
                    assert!(report.inspected_runs >= 624);
                    heavy_decision = report.decision;
                }
                if trial < 2 {
                    continue;
                }
                match (tick, report.published) {
                    (0, _) => heavy.push(ms),
                    (_, 1) => normal_published.push(ms),
                    _ => normal_held.push(ms),
                }
            }
        }
        let results = serde_json::json!({
            "heavy_family_tick": summary(&heavy),
            "normal_published_ticks": summary(&normal_published),
            "normal_held_ticks": summary(&normal_held),
            "heavy_decision": format!("{heavy_decision:?}"),
        });
        let result = serde_json::json!({"threads":count,"runs":runs,"projects":8,"lanes":lanes,"heavy_member_runs":624,"results":results,"warmup_trials":2,"trials":30});
        std::fs::write(
            std::path::Path::new(&output).join(format!("c0-run-{execution}-{count}.json")),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
    }
    #[test]
    #[ignore = "manual final C-0 experiment"]
    fn c0_1000() {
        measure(1000)
    }
    #[test]
    #[ignore = "manual final C-0 experiment"]
    fn c0_2000() {
        measure(2000)
    }
    #[test]
    #[ignore = "manual final C-0 experiment"]
    fn c0_5000() {
        measure(5000)
    }
}
