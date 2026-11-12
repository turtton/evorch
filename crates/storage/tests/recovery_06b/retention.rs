use super::*;
use std::collections::BTreeMap;
use storage::thread_retention::{
    BindingInput, BlockReason, ExploreBudget, FamilyState, FinalizeBudget, FinalizeOutcome,
    ReclaimBudget, RetentionInput, SidebarThread, ThreadRetentionPolicy,
};
struct Retention {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    db: Database,
    exclusive: storage::ExclusiveLeases,
    input: RetentionInput,
}
impl Retention {
    fn new(state: &str, pin: bool) -> Self {
        let f = Fixture::new();
        let request = f.seed(false);
        if matches!(state, "prepared" | "activated" | "released") {
            f.claim(&request);
        }
        if state == "activated" {
            f.storage.handle().activate_recovery_claim(request).unwrap();
        } else if state == "released" {
            f.storage
                .handle()
                .release_recovery_claim("claim".into(), "operator".into())
                .unwrap();
        } else if state == "partial" {
            f.sql()
                .execute_batch("UPDATE recovery_manifests SET committed=0")
                .unwrap();
        }
        f.sql().execute_batch(
            "INSERT INTO threads(thread_id,project_root,root_run_id,archived,created_at_ns,updated_at_ns,source)
             VALUES ('A','/project','run-a',1,1,1,'host'),('B','/project','run-b',1,1,1,'host');
             INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth)
             VALUES ('run-a','A','run-a',0),('run-b','B','run-b',0)"
        ).unwrap();
        let Fixture {
            _dir,
            config,
            storage,
        } = f;
        storage.close();
        let root = _dir.path().join("owners");
        let exclusive = storage::try_exclusive(&config.db_path, &root)
            .unwrap()
            .unwrap();
        let db = Database::open(&config).unwrap();
        Self {
            _dir,
            config,
            db,
            exclusive,
            input: RetentionInput {
                threads: ["A", "B"]
                    .into_iter()
                    .map(|id| SidebarThread {
                        thread_id: id.into(),
                        archived: true,
                        archived_at_s: None,
                        pinned: pin && id == "A",
                        updated_at_s: 1,
                        created_at_s: 1,
                        archive_revision: None,
                        pin_revision: None,
                    })
                    .collect(),
                sidebar_path: "/sidebar".into(),
                ownership_root: root.to_string_lossy().into(),
                now_ns: 100 * storage::thread_retention::NANOS_PER_DAY,
            },
        }
    }
    fn sql(&self) -> rusqlite::Connection {
        rusqlite::Connection::open(&self.config.db_path).unwrap()
    }
    fn settle(&self) -> Vec<(String, FamilyState)> {
        loop {
            let states = self
                .db
                .retention_explore(
                    &self.input,
                    &ThreadRetentionPolicy::default(),
                    ExploreBudget { max_edges: 128 },
                )
                .unwrap();
            if states
                .iter()
                .all(|(_, s)| !matches!(s, FamilyState::Exploring { .. }))
            {
                return states;
            }
        }
    }
    fn finalize(&self, key: &str, budget: u64) -> FinalizeOutcome {
        self.db
            .retention_finalize(
                key,
                &self.input,
                &ThreadRetentionPolicy::default(),
                &BTreeMap::new(),
                &BindingInput {
                    ownership_root: self.input.ownership_root.clone(),
                    sidebar_path: self.input.sidebar_path.clone(),
                },
                FinalizeBudget { max_edges: budget },
            )
            .unwrap()
    }
    fn tombstones(&self) -> u64 {
        self.sql()
            .query_row("SELECT COUNT(*) FROM run_tombstones", [], |r| r.get(0))
            .unwrap()
    }
    fn reclaim_and_reopen(self) -> Self {
        let Self {
            _dir,
            config,
            db,
            exclusive,
            input,
        } = self;
        drop(db);
        let shared = exclusive.downgrade(std::time::Duration::ZERO).unwrap();
        let storage = Storage::open_with_lease(config.clone(), shared.db_lease().unwrap()).unwrap();
        loop {
            let progress = storage
                .handle()
                .reclaim_step(ReclaimBudget::default())
                .unwrap();
            assert!(progress.last_error.is_none(), "{progress:?}");
            if progress.jobs_db_pending == 0 {
                break;
            }
        }
        storage.close();
        drop(shared);
        let exclusive =
            storage::try_exclusive(&config.db_path, std::path::Path::new(&input.ownership_root))
                .unwrap()
                .unwrap();
        let db = Database::open(&config).unwrap();
        Self {
            _dir,
            config,
            db,
            exclusive,
            input,
        }
    }
}
#[test]
fn committed_component_pin_is_a_cross_family_protection() {
    let f = Retention::new("committed", true);
    let states = f.settle();
    assert_eq!(states.len(), 1);
    assert_eq!(
        f.finalize(&states[0].0, u64::MAX),
        FinalizeOutcome::Blocked(BlockReason::Pinned)
    );
    assert_eq!(f.tombstones(), 0);
}
#[test]
fn due_component_is_indivisible_even_when_the_budget_ends_between_members() {
    for state in ["committed", "released"] {
        let f = Retention::new(state, false);
        let states = f.settle();
        assert_eq!(states.len(), 1);
        assert!(matches!(
            f.finalize(&states[0].0, 1),
            FinalizeOutcome::OverBudget { .. }
        ));
        assert_eq!(f.tombstones(), 0);
        let FinalizeOutcome::Confirmed(family) = f.finalize(&states[0].0, u64::MAX) else {
            panic!("not confirmed")
        };
        assert_eq!(family.runs, ["run-a", "run-b"]);
        assert_eq!(f.tombstones(), 2);
        let f = f.reclaim_and_reopen();
        assert!(f.db.recovery_manifests().unwrap().is_empty());
        assert!(f.db.recovery_claims().unwrap().is_empty());
        assert!(f.db.run_context("run-a").unwrap().is_none());
        assert!(f.db.run_context("run-b").unwrap().is_none());
    }
}
#[test]
fn prepared_claim_is_an_owner_hold_not_a_partial_deletion() {
    let f = Retention::new("prepared", false);
    let states = f.settle();
    assert_eq!(
        f.finalize(&states[0].0, u64::MAX),
        FinalizeOutcome::Blocked(BlockReason::Owner("recovery claim pending".into()))
    );
    assert_eq!(f.tombstones(), 0);
}
#[test]
fn capture_committed_after_explore_invalidates_the_fixed_point() {
    let f = Retention::new("partial", false);
    assert_eq!(f.settle().len(), 2);
    f.sql()
        .execute_batch("UPDATE recovery_manifests SET committed=1")
        .unwrap();
    assert!(matches!(
        f.finalize("B", u64::MAX),
        FinalizeOutcome::Stale { .. }
    ));
    assert_eq!(f.tombstones(), 0);
}
#[test]
fn retired_manifest_does_not_join_a_pinned_run_to_an_independent_due_run() {
    let f = Retention::new("activated", true);
    let states = f.settle();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].0, "B", "pinned A is not a deletion candidate");
    let before = f.db.resume_intent("run-a").unwrap();
    let context = f.db.run_context("run-a").unwrap();
    assert!(matches!(
        f.finalize("B", u64::MAX),
        FinalizeOutcome::Confirmed(_)
    ));
    assert_eq!(f.tombstones(), 1);
    let f = f.reclaim_and_reopen();
    f.settle();
    assert_eq!(f.db.resume_intent("run-a").unwrap(), before);
    assert_eq!(f.db.run_context("run-a").unwrap(), context);
}
