//! Shared fixtures: a database, an ownership root and a sidebar file in one
//! temporary directory, and archived threads written straight into the tables.

use std::path::PathBuf;

use rusqlite::{Connection, params};
use storage::thread_retention::{
    RetentionInput, SidebarThread, ThreadRetentionPolicy, TombstoneSet,
};
use storage::{Database, ExclusiveLeases, StorageConfig};

use super::{StartupBudget, StartupPaths, StartupReport, run_exclusive_phase, startup_input};
use crate::ownership::{Lease, Registry, ThreadOwner};
use crate::scratch::ThreadScratch;

mod binding;
mod entries;
mod executor;
mod gc;
pub(super) mod guard;
mod startup;

pub(super) const DAY_NS: i64 = 86_400 * 1_000_000_000;
/// "Now" is day 100; threads archived at second 0 are long past the 30-day period.
pub(super) const NOW_NS: i64 = 100 * DAY_NS;

pub(super) fn policy() -> ThreadRetentionPolicy {
    ThreadRetentionPolicy::default()
}

pub(super) struct World {
    pub dir: tempfile::TempDir,
    pub db_path: PathBuf,
    pub root: PathBuf,
    pub sidebar: PathBuf,
}

impl World {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let world = Self {
            db_path: dir.path().join("events.db"),
            root: dir.path().join("ownership"),
            sidebar: dir.path().join("sidebar.json"),
            dir,
        };
        std::fs::create_dir_all(&world.root).unwrap();
        std::fs::write(&world.sidebar, "{}").unwrap();
        drop(Database::open(&world.storage_config()).unwrap());
        world
    }

    /// Another database and sidebar sharing this world's ownership root.
    pub fn sibling_sharing_root(&self, name: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let world = Self {
            db_path: dir.path().join(format!("{name}.db")),
            root: self.root.clone(),
            sidebar: dir.path().join("sidebar.json"),
            dir,
        };
        std::fs::write(&world.sidebar, "{}").unwrap();
        drop(Database::open(&world.storage_config()).unwrap());
        world
    }

    pub fn storage_config(&self) -> StorageConfig {
        StorageConfig {
            db_path: self.db_path.clone(),
            ..StorageConfig::default()
        }
    }

    pub fn database(&self) -> Database {
        Database::open(&self.storage_config()).unwrap()
    }

    pub fn conn(&self) -> Connection {
        let conn = Connection::open(&self.db_path).unwrap();
        conn.pragma_update(None, "foreign_keys", true).unwrap();
        conn
    }

    pub fn count(&self, sql: &str) -> i64 {
        self.conn().query_row(sql, [], |row| row.get(0)).unwrap()
    }

    /// An archived thread with one saved root run bound to it.
    pub fn archived_thread(&self, thread: &str, run: &str) {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO threads(thread_id, project_root, root_run_id, archived, created_at_ns, \
             updated_at_ns, source) VALUES (?1, '/p', ?2, 1, 1, 1, 'host')",
            params![thread, run],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO run_contexts(run_id, role, name, parent_run_id, config_json, \
             checkpoints_json, terminal_phase, restorable, updated_at_ns, \
             messages_len, checkpoints_len, config_len) VALUES (?1, 'Worker', ?2, NULL, '{}', '[]', \
             'Done', 1, 1, 2, 2, 2)",
            params![run, format!("chat:Worker:{thread}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO thread_runs(run_id, thread_id, root_run_id, depth) VALUES (?1, ?2, ?1, 0)",
            params![run, thread],
        )
        .unwrap();
    }

    pub fn paths(&self) -> StartupPaths {
        StartupPaths {
            db_path: self.db_path.clone(),
            ownership_root: self.root.clone(),
            sidebar_path: self.sidebar.clone(),
            artifacts_dir: None,
        }
    }

    /// The sidebar as read after taking the exclusive locks: `archived` threads were
    /// archived at second 0, the others are active.
    pub fn input(&self, archived: &[&str], active: &[&str]) -> RetentionInput {
        let thread = |id: &str, archived: bool| SidebarThread {
            thread_id: id.to_owned(),
            archived,
            archived_at_s: archived.then_some(0),
            pinned: false,
            updated_at_s: 1,
            created_at_s: 1,
            archive_revision: None,
            pin_revision: None,
        };
        let threads = archived
            .iter()
            .map(|id| thread(id, true))
            .chain(active.iter().map(|id| thread(id, false)))
            .collect();
        startup_input(&self.paths(), threads, NOW_NS).unwrap()
    }

    pub fn exclusive(&self) -> ExclusiveLeases {
        storage::try_exclusive(&self.db_path, &self.root)
            .unwrap()
            .expect("no other holder in tests")
    }

    /// One whole phase: take the exclusive locks, run, release.
    pub fn phase(&self, policy: &ThreadRetentionPolicy, input: &RetentionInput) -> StartupReport {
        self.phase_with(policy, input, &StartupBudget::default())
    }

    pub fn phase_with(
        &self,
        policy: &ThreadRetentionPolicy,
        input: &RetentionInput,
        budget: &StartupBudget,
    ) -> StartupReport {
        let leases = self.exclusive();
        run_exclusive_phase(&leases, &self.paths(), policy, input, budget, NOW_NS).unwrap()
    }

    pub fn registry(&self) -> Registry {
        Registry::open(&self.root.join("owners.db")).unwrap()
    }

    /// An owner row; `active` marks a turn in flight.
    pub fn owner_row(&self, thread: &str, active: bool, expires_at_ms: u64) {
        let mut owner = ThreadOwner::new(
            thread.to_owned(),
            Lease {
                owner_id: "previous-process".into(),
                generation: 1,
                expires_at: expires_at_ms,
            },
        );
        owner.active_turn = active;
        self.registry().start(&owner).unwrap();
    }

    pub fn owner_rows(&self) -> Vec<String> {
        self.registry()
            .list()
            .unwrap()
            .into_iter()
            .map(|owner| owner.thread_id)
            .collect()
    }

    pub fn scratch_exists(&self, thread: &str) -> bool {
        self.scratch().thread_dir_exists(thread).unwrap()
    }

    pub fn scratch(&self) -> ThreadScratch {
        ThreadScratch::for_ownership_root(&self.root).unwrap()
    }

    /// Create `thread`'s scratch directory with a file in it.
    pub fn scratch_dir(&self, thread: &str) -> PathBuf {
        let dir = self
            .scratch()
            .prepare(&crate::scratch::ScratchKey::Thread(thread.into()))
            .unwrap();
        std::fs::write(dir.join("note.txt"), "x").unwrap();
        dir
    }

    pub fn tombstones(&self) -> TombstoneSet {
        self.database().tombstones().unwrap()
    }

    pub fn root_binding_file(&self) -> PathBuf {
        self.root.join(super::binding::ROOT_BINDING_FILE)
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // The scratch parent lives under /var/tmp, outside the temporary directory.
        if let Ok(scratch) = ThreadScratch::for_ownership_root(&self.root) {
            let _ = std::fs::remove_dir_all(scratch.parent());
        }
    }
}

pub(super) fn ran(report: &StartupReport) -> RanView<'_> {
    match &report.outcome {
        super::StartupOutcome::Ran {
            confirmed,
            blocked,
            owners_removed,
            scratch,
            owner_registry_error,
            errors,
            families,
            backfill,
            stale,
            gc,
            ..
        } => RanView {
            confirmed: confirmed
                .iter()
                .map(|family| family.family_key.as_str())
                .collect(),
            blocked,
            owners_removed: *owners_removed,
            scratch,
            owner_registry_error: owner_registry_error.as_deref(),
            errors,
            families,
            remaining_backfill: backfill.remaining_unconverted + backfill.remaining_unattributed,
            stale,
            gc: gc.as_ref(),
        },
        other => panic!("phase was skipped: {other:?}"),
    }
}

pub(super) struct RanView<'a> {
    pub confirmed: Vec<&'a str>,
    pub blocked: &'a [(String, storage::thread_retention::BlockReason)],
    pub owners_removed: usize,
    pub scratch: &'a super::startup::ScratchReport,
    pub owner_registry_error: Option<&'a str>,
    pub errors: &'a [String],
    pub families: &'a [(String, storage::thread_retention::FamilyState)],
    pub remaining_backfill: u64,
    pub stale: &'a [String],
    pub gc: Option<&'a crate::artifacts::GcReport>,
}
