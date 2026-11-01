//! The order of the GUI start-up and what it hands to the workbench.

use std::time::Duration;

use gui::startup::{StartupEvent, StartupPlan, run_startup};
use runtime::thread_retention::{SkipReason, Stage, StartupOutcome, StartupProgress};
use storage::thread_retention::{LegacyArchivedPolicy, ThreadRetentionPolicy};
use tempfile::TempDir;
use workspace_ui::{ProjectId, SidebarState, ThreadId};

struct Fixture {
    dir: TempDir,
}

impl Fixture {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn plan(&self, run_phase: bool) -> StartupPlan {
        StartupPlan {
            db_path: self.dir.path().join("events.db"),
            ownership_root: self.dir.path().join("threads"),
            sidebar_path: Some(self.dir.path().join("sidebar.json")),
            artifacts_dir: Some(self.dir.path().join("artifacts")),
            run_phase,
        }
    }

    /// A sidebar with one live and one thread archived by a version that kept no time.
    fn write_sidebar(&self) {
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("p");
        sidebar
            .add_project(project.clone(), "P", self.dir.path())
            .unwrap();
        sidebar.select_project(&project).unwrap();
        sidebar
            .create_thread(ThreadId::new("live"), project.clone(), "Live")
            .unwrap();
        sidebar
            .create_thread(ThreadId::new("old"), project, "Old")
            .unwrap();
        sidebar.threads[1].archived = true;
        workspace_ui::save_sidebar(&sidebar, &self.dir.path().join("sidebar.json")).unwrap();
    }
}

fn policy(days: u32) -> impl FnOnce(&SidebarState) -> ThreadRetentionPolicy {
    move |_| ThreadRetentionPolicy {
        archived_delete_after_days: days,
        legacy: LegacyArchivedPolicy::Immediate,
    }
}

fn collect(plan: &StartupPlan, days: u32) -> (Vec<StartupEvent>, gui::startup::StartupPrelude) {
    let mut events = Vec::new();
    let prelude = run_startup(plan, policy(days), &mut |event| events.push(event)).unwrap();
    (events, prelude)
}

#[test]
fn the_start_takes_exclusive_locks_reads_the_sidebar_runs_the_phase_and_shares_the_locks() {
    // Given: a sidebar with an archived thread of an older version.
    let fixture = Fixture::new();
    fixture.write_sidebar();

    // When: the start-up runs with a 30-day period.
    let (events, prelude) = collect(&fixture.plan(true), 30);

    // Then: the order is exclusive → sidebar → phase stages → shared.
    assert_eq!(events.first(), Some(&StartupEvent::ExclusiveTaken));
    assert_eq!(events[1], StartupEvent::SidebarLoaded { threads: 2 });
    assert_eq!(events.last(), Some(&StartupEvent::Shared));
    let stages: Vec<Stage> = events
        .iter()
        .filter_map(|event| match event {
            StartupEvent::Phase(StartupProgress::StageStarted(stage)) => Some(*stage),
            _ => None,
        })
        .collect();
    assert_eq!(
        stages,
        [
            Stage::Backfill,
            Stage::Explore,
            Stage::Finalize,
            Stage::Owners,
            Stage::Scratch,
            Stage::Oversized,
            Stage::Vacuum,
        ]
    );
    let first_phase = events
        .iter()
        .position(|event| matches!(event, StartupEvent::Phase(_)))
        .unwrap();
    let last_phase = events
        .iter()
        .rposition(|event| matches!(event, StartupEvent::Phase(_)))
        .unwrap();
    assert!(1 < first_phase && last_phase < events.len() - 1);

    // And: the archived legacy thread was confirmed, so the re-read tombstones hold it.
    assert!(matches!(prelude.report.outcome, StartupOutcome::Ran { .. }));
    assert_eq!(
        gui::app::confirmed_threads(&prelude.report)
            .into_iter()
            .collect::<Vec<_>>(),
        ["old"]
    );
    assert!(prelude.tombstones.contains_thread("old"));
    assert!(!prelude.tombstones.contains_thread("live"));
    assert!(events.iter().any(|event| matches!(
        event,
        StartupEvent::Phase(StartupProgress::FamilyConfirmed { .. })
    )));
}

#[test]
fn a_period_of_zero_days_confirms_nothing() {
    let fixture = Fixture::new();
    fixture.write_sidebar();

    let (_, prelude) = collect(&fixture.plan(true), 0);

    assert!(gui::app::confirmed_threads(&prelude.report).is_empty());
    assert!(prelude.tombstones.is_empty());
}

#[test]
fn another_process_holding_a_lock_skips_the_phase_and_the_start_goes_on() {
    // Given: another window holds the locks shared.
    let fixture = Fixture::new();
    fixture.write_sidebar();
    let plan = fixture.plan(true);
    let other =
        storage::shared_with_wait(&plan.db_path, &plan.ownership_root, Duration::from_secs(1))
            .unwrap();

    // When: this process starts.
    let (events, prelude) = collect(&plan, 30);

    // Then: no stage ran, nothing was confirmed, and the locks are shared.
    assert_eq!(
        events,
        [StartupEvent::ExclusiveUnavailable, StartupEvent::Shared]
    );
    assert!(matches!(
        prelude.report.outcome,
        StartupOutcome::Skipped {
            reason: SkipReason::NoExclusive,
            ..
        }
    ));
    assert!(prelude.tombstones.is_empty());
    assert!(gui::app::startup_notice(&prelude.report, prelude.binding.as_ref()).is_none());
    drop(other);
}

#[test]
fn the_demo_takes_shared_locks_only() {
    let fixture = Fixture::new();
    fixture.write_sidebar();

    let (events, prelude) = collect(&fixture.plan(false), 30);

    assert_eq!(events, [StartupEvent::Shared]);
    assert!(prelude.sidebar.is_none());
    assert!(prelude.tombstones.is_empty());
}

#[test]
fn a_failing_start_releases_the_locks() {
    // Given: a sidebar file that cannot be read.
    let fixture = Fixture::new();
    std::fs::write(fixture.dir.path().join("sidebar.json"), b"{ not json").unwrap();
    let plan = fixture.plan(true);

    // When: the start-up runs.
    let error = run_startup(&plan, policy(30), &mut |_| {}).err();

    // Then: it reports the failure and a later start can take the locks again.
    assert!(error.is_some());
    assert!(
        storage::try_exclusive(&plan.db_path, &plan.ownership_root)
            .unwrap()
            .is_some()
    );
}

#[test]
fn a_restart_keeps_the_tombstone_and_confirms_nothing_twice() {
    // Given: a first start that deleted the archived thread.
    let fixture = Fixture::new();
    fixture.write_sidebar();
    let plan = fixture.plan(true);
    let (_, first) = collect(&plan, 30);
    drop(first);

    // When: the same sidebar is used for a second start.
    let (_, second) = collect(&plan, 30);

    // Then: the tombstone is still there and nothing is confirmed twice.
    assert!(second.tombstones.contains_thread("old"));
    assert!(gui::app::confirmed_threads(&second.report).is_empty());
}

#[test]
fn a_panic_in_the_startup_thread_becomes_a_startup_error() {
    // Given: a policy source that panics, as a broken configuration might.
    let fixture = Fixture::new();
    fixture.write_sidebar();
    let receiver = gui::startup::spawn_startup(
        fixture.plan(true),
        |_| panic!("configuration exploded"),
        std::sync::Arc::new(|| {}),
    );

    // When: the messages are read until the thread reports its end.
    let mut events = Vec::new();
    let result = loop {
        match receiver.recv().expect("the thread always reports its end") {
            gui::startup::StartupMessage::Event(event) => events.push(event),
            gui::startup::StartupMessage::Finished(result) => break *result,
        }
    };

    // Then: the events up to the panic arrived and the panic is an error.
    assert_eq!(events.first(), Some(&StartupEvent::ExclusiveTaken));
    let error = result.err().expect("an error");
    assert!(
        error.to_string().contains("configuration exploded"),
        "{error}"
    );
}

/// Two archived threads of one family (the child forked from the parent), known to the
/// database; `edit` changes the sidebar before it is saved. Returns the start-up's
/// prelude and the GUI scheduler's statuses.
fn held_family(
    edit: impl FnOnce(&mut SidebarState),
) -> (
    gui::startup::StartupPrelude,
    std::collections::BTreeMap<String, storage::thread_retention::RetentionStatus>,
) {
    let fixture = Fixture::new();
    let plan = fixture.plan(true);
    let config = storage::StorageConfig {
        db_path: plan.db_path.clone(),
        ..Default::default()
    };
    {
        let storage = storage::Storage::open(config.clone()).unwrap();
        let thread = |id: &str, parent: Option<&str>| storage::threads::ThreadUpsert {
            thread_id: id.into(),
            parent_thread_id: parent.map(str::to_owned),
            lineage_kind: parent.map(|_| storage::threads::ThreadLineageKind::Fork),
            root_run_id: Some(format!("run-{id}")),
            created_at_ns: 1,
            updated_at_ns: 1,
            ..Default::default()
        };
        storage
            .handle()
            .upsert_threads_host(&[thread("parent", None), thread("child", Some("parent"))])
            .unwrap();
    }
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("p");
    sidebar
        .add_project(project.clone(), "P", fixture.dir.path())
        .unwrap();
    sidebar.select_project(&project).unwrap();
    for id in ["parent", "child"] {
        sidebar
            .create_thread(ThreadId::new(id), project.clone(), id)
            .unwrap();
        sidebar.threads.last_mut().unwrap().archived = true;
    }
    sidebar.threads[1].parent_thread_id = Some(ThreadId::new("parent"));
    edit(&mut sidebar);
    workspace_ui::save_sidebar(&sidebar, plan.sidebar_path.as_ref().unwrap()).unwrap();

    let (_, prelude) = collect(&plan, 30);
    let scheduler = gui::app::StorageRetentionScheduler {
        storage_config: config,
        ownership_root: plan.ownership_root.clone(),
        sidebar_path: plan.sidebar_path.clone().unwrap(),
        policy: std::sync::Arc::new(std::sync::Mutex::new(ThreadRetentionPolicy {
            archived_delete_after_days: 30,
            legacy: LegacyArchivedPolicy::Immediate,
        })),
    };
    use gui::app::RetentionScheduler as _;
    let snapshot = scheduler
        .schedule(gui::startup::sidebar_threads(&sidebar), 1_000_000_000_000)
        .unwrap();
    drop(fixture);
    (prelude, snapshot.statuses)
}

#[test]
fn the_shown_status_of_a_held_family_comes_from_the_real_scheduler() {
    // Given: the child of an archived family is pinned.
    // When: the start-up runs (it explores the family but cannot confirm it) and the
    // GUI's scheduler computes the statuses.
    let (prelude, statuses) = held_family(|sidebar| sidebar.threads[1].pinned = true);

    // Then: both threads read as held for the pin, not as due.
    assert!(prelude.tombstones.is_empty());
    for id in ["parent", "child"] {
        assert_eq!(
            statuses[id],
            storage::thread_retention::RetentionStatus::Blocked(
                storage::thread_retention::BlockReason::Pinned
            ),
            "{id}"
        );
    }
}

#[test]
fn a_family_with_a_pending_child_is_not_deleted_and_reads_as_held() {
    // Given: the child's archive state is still pending (saved neutral: not archived).
    // When: the start-up runs and the statuses are computed.
    let (prelude, statuses) = held_family(|sidebar| {
        sidebar.threads[1].archived = false;
        sidebar.threads[1].state_pending = true;
    });

    // Then: nothing is confirmed, and the family reads as held by the unsettled child.
    assert!(prelude.tombstones.is_empty());
    assert!(gui::app::confirmed_threads(&prelude.report).is_empty());
    assert_eq!(
        statuses["parent"],
        storage::thread_retention::RetentionStatus::Blocked(
            storage::thread_retention::BlockReason::FamilyMemberNotDue("child".into())
        )
    );
}

#[test]
fn the_startup_collects_unused_artifact_files_and_keeps_every_captured_one() {
    // Given: the GUI's artifact directory next to the database. One capture recorded
    // its reference as the GUI does, one was made by a host that records none (the
    // meta file alone), and a blob file no meta file points at.
    use runtime::artifacts::{ArtifactStore, CaptureRequest, store_with_refs};
    let fixture = Fixture::new();
    fixture.write_sidebar();
    let plan = fixture.plan(true);
    let artifacts = plan.artifacts_dir.clone().unwrap();
    let workspace = fixture.dir.path().join("work");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("a.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-a").unwrap();
    std::fs::write(workspace.join("b.png"), b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-b").unwrap();
    let request = |run: &str| CaptureRequest {
        title: "t",
        caption: None,
        run_id: run.into(),
        root_run_id: run.into(),
        thread_id: Some("live".into()),
    };
    let (recorded, unrecorded) = {
        // The writer holds the database; it is closed before the start-up takes the locks.
        let storage = storage::Storage::open(storage::StorageConfig {
            db_path: plan.db_path.clone(),
            ..Default::default()
        })
        .unwrap();
        let recorded = store_with_refs(&artifacts, storage.handle())
            .unwrap()
            .capture(
                &workspace,
                None,
                std::path::Path::new("a.png"),
                request("run-a"),
            )
            .unwrap();
        let unrecorded = ArtifactStore::new(&artifacts)
            .unwrap()
            .capture(
                &workspace,
                None,
                std::path::Path::new("b.png"),
                request("run-b"),
            )
            .unwrap();
        (recorded, unrecorded)
    };
    let store = ArtifactStore::new(&artifacts).unwrap();
    let kept = [store.blob_path(&recorded), store.blob_path(&unrecorded)];
    let orphan = artifacts
        .join("blobs")
        .join(format!("{}.png", "ab".repeat(32)));
    std::fs::write(&orphan, b"nobody refers to this").unwrap();

    // When: the start-up runs.
    let (events, prelude) = collect(&plan, 30);

    // Then: the collection ran as a stage of the exclusive phase and removed only the
    // blob nothing refers to; both captured blobs and their meta files are still there.
    assert!(
        events.contains(&StartupEvent::Phase(StartupProgress::StageStarted(
            Stage::ArtifactGc
        )))
    );
    let StartupOutcome::Ran { gc: Some(gc), .. } = &prelude.report.outcome else {
        panic!(
            "the phase must report the collection: {:?}",
            prelude.report.outcome
        );
    };
    assert!(gc.refs_complete, "{gc:?}");
    assert_eq!(gc.blobs_unlinked, 1, "{gc:?}");
    assert!(!orphan.exists());
    for blob in &kept {
        assert!(blob.exists(), "{} was removed", blob.display());
    }
    assert!(store.load(&recorded.artifact_id).is_ok());
    assert!(store.load(&unrecorded.artifact_id).is_ok());
    // And: the notice tells about it.
    let (_, summary) = gui::app::startup_notice(&prelude.report, prelude.binding.as_ref())
        .expect("a removed file is worth a notice");
    assert!(
        summary.contains("removed 1 unused artifact files"),
        "{summary}"
    );
}
