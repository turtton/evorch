//! Auto-archive fixtures own synthetic DB, sidebar and attachments. No wall-clock verdicts.
mod common;
use std::sync::{Arc, Mutex};
use storage::thread_retention::ExclusiveFixtureStorage;
use storage::thread_retention::{AutoArchiveAuthority, AutoArchivePolicy};
use storage::{Database, StorageConfig, StorageHandle};
use storage::{ExclusiveLeases, try_exclusive};
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
            rt: tokio::runtime::Runtime::new().unwrap(),
        }
    }
    fn leases(&self) -> ExclusiveLeases {
        try_exclusive(&self.config.db_path, self.dir.path())
            .unwrap()
            .unwrap()
    }
    fn drain(&self, handle: &StorageHandle) {
        let db = Database::open(&self.config).unwrap();
        for _ in 0..100 {
            let r = handle
                .apply_activity_page(db.activity_page().unwrap())
                .unwrap();
            assert!(r.inspected_rows <= 500);
            assert!(r.decode_bytes <= 2_097_152);
            if !r.backfill_pending && r.pending_sources == 0 {
                return;
            }
        }
        panic!("bounded fixture did not drain");
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
        .with_mirror(handle);
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
    fn drive(
        &self,
        c: &mut AutoArchiveCoordinator,
        h: &Arc<Mutex<CommandHost>>,
        a: &AutoArchiveAuthority<'_>,
        now: i64,
    ) -> RetentionReport {
        for _ in 0..100 {
            let r = c.tick(h, Some(a), now);
            assert!(r.published <= 1);
            assert!(r.attachment_calls <= 1);
            assert!(r.scratch_hints <= 1);
            assert!(r.failures.is_empty(), "{:?}", r.failures);
            if r.published > 0
                || (!r.holds.is_empty() && !r.holds.contains_key("FamilyInspectionPending"))
            {
                return r;
            }
        }
        panic!("family never completed");
    }
}
#[test]
fn no_authority_does_not_touch_host_sidebar_or_attachment_locks() {
    let w = World::new(1);
    let leases = w.leases();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    w.drain(&storage.handle());
    let host = w.host(storage.handle());
    let mut c = w.coordinator();
    let before = std::fs::read(w.store.path()).unwrap();
    let sidebar = w.store.lock().unwrap();
    let attachment = sandbox::attachments::AttachmentRoot::at(w.dir.path().join("attachments"))
        .lock_thread("thread")
        .unwrap();
    let foreground = host.lock().unwrap();
    let r = c.tick(&host, None, 8 * DAY);
    assert_eq!(r.holds.get("AuthorityUnavailable"), Some(&1));
    assert_eq!(r.published, 0);
    assert_eq!(r.attachment_calls, 0);
    assert_eq!(r.scratch_hints, 0);
    assert_eq!(r.inspected_runs, 0);
    assert_eq!(std::fs::read(w.store.path()).unwrap(), before);
    drop((foreground, attachment, sidebar));
}
#[test]
fn due_family_publishes_once_and_keeps_real_commit_time_on_retry() {
    let w = World::new(3);
    let leases = w.leases();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    w.drain(&storage.handle());
    let host = w.host(storage.handle());
    let a = AutoArchiveAuthority::new(&leases);
    let mut c = w.coordinator();
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 7 * DAY - 1);
    assert_eq!(r.published, 0);
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 7 * DAY);
    assert_eq!(r.published, 1);
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
fn late_bound_member_runs_control_the_family_maximum_and_heavy_family_progresses() {
    let w = World::new(624);
    let leases = w.leases();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    w.drain(&storage.handle());
    let conn = rusqlite::Connection::open(&w.config.db_path).unwrap();
    conn.execute(
        "UPDATE events SET wall_clock_ns=?1 WHERE run_id='run-624'",
        [6 * DAY],
    )
    .unwrap();
    w.drain(&storage.handle());
    let host = w.host(storage.handle());
    let a = AutoArchiveAuthority::new(&leases);
    let mut c = w.coordinator();
    let r = w.drive(&mut c, &host, &a, 8 * DAY);
    assert_eq!(r.published, 0);
    assert!(!w.archived());
    let r = w.drive(&mut c, &host, &a, 13 * DAY);
    assert_eq!(r.published, 1);
    assert!(w.archived());
}
#[test]
fn held_foreground_mutex_defers_background_without_changing_foreground_contract() {
    let w = World::new(1);
    let leases = w.leases();
    let storage = ExclusiveFixtureStorage::open(w.config.clone(), w.dir.path(), &leases).unwrap();
    w.drain(&storage.handle());
    let host = w.host(storage.handle());
    let a = AutoArchiveAuthority::new(&leases);
    let mut c = w.coordinator();
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
