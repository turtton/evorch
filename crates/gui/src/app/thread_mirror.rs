//! Mirrors the sidebar's thread definitions into the storage `threads` table
//! (ADR 0041), where the runtime's thread tools find titles, projects and
//! lineage. `sidebar.json` stays the source; the mirror only writes.
//!
//! Values the sidebar does not know are sent as `None`, which storage never
//! lets erase a stored value, so what the runtime recorded (such as a root run
//! the sidebar has not bound yet) survives the mirror.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use storage::threads::{ThreadLineageKind, ThreadUpsert};
use workspace_ui::{LineageKind, ProjectId, ThreadChatRole, ThreadRecord};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

const SYNC_INTERVAL: Duration = Duration::from_secs(1);

const NANOS_PER_SECOND: i64 = 1_000_000_000;

/// Writer link for the mirror. Definitions are sent without waiting for the
/// writer, so the render thread never blocks on SQLite.
pub(crate) struct ThreadMirrorLink {
    handle: storage::StorageHandle,
    /// The definition last handed to the writer, by thread ID. A definition
    /// the writer later failed to store is sent again only when it changes or
    /// the GUI restarts, as with the usage ledger.
    mirrored: HashMap<String, ThreadUpsert>,
    canonical_roots: CanonicalRoots,
    next_sync: Option<Instant>,
}

/// Canonical forms of project roots, so the filesystem is asked once per root.
#[derive(Default)]
struct CanonicalRoots(HashMap<PathBuf, String>);

impl CanonicalRoots {
    /// The project root as the runtime binds runs to it: canonical, as lossy
    /// UTF-8. `None` while it cannot be resolved (the directory is gone), so
    /// a path that is not canonical never becomes the visibility key and a
    /// stored root is kept; it is tried again on the next sync.
    fn get(&mut self, repo_root: &Path) -> Option<String> {
        if let Some(root) = self.0.get(repo_root) {
            return Some(root.clone());
        }
        let canonical = std::fs::canonicalize(repo_root).ok()?;
        let root = canonical.to_string_lossy().into_owned();
        self.0.insert(repo_root.to_path_buf(), root.clone());
        Some(root)
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Mirror the sidebar's thread definitions into storage as they change.
    pub fn with_thread_mirror(mut self, handle: storage::StorageHandle) -> Self {
        self.thread_mirror = Some(ThreadMirrorLink {
            handle,
            mirrored: HashMap::new(),
            canonical_roots: CanonicalRoots::default(),
            next_sync: None,
        });
        self
    }

    /// Definitions another process saved were already mirrored by it; record
    /// them as sent so this process does not write them again.
    pub(super) fn note_threads_mirrored(&mut self, ids: &[String]) {
        let Some(link) = self.thread_mirror.as_mut() else {
            return;
        };
        for thread in self
            .sidebar
            .threads
            .iter()
            .filter(|thread| ids.contains(&thread.id.to_string()))
        {
            let root = self
                .sidebar
                .projects
                .iter()
                .find(|project| project.id == thread.project_id)
                .and_then(|project| link.canonical_roots.get(&project.repo_root));
            if let Some(upsert) = thread_upsert(thread, root) {
                link.mirrored.insert(upsert.thread_id.clone(), upsert);
            }
        }
    }

    /// Sends what changed now instead of at the next tick: an archive or unarchive
    /// has an effect on the conversation index that should not lag behind it.
    pub(super) fn force_thread_mirror(&mut self) {
        if let Some(link) = self.thread_mirror.as_mut() {
            link.next_sync = None;
        }
        self.sync_thread_mirror(Instant::now());
    }

    /// Send the definitions that changed since the last sync, at most once per
    /// [`SYNC_INTERVAL`]. The first sync sends every thread.
    pub(super) fn sync_thread_mirror(&mut self, now: Instant) {
        let Some(link) = self.thread_mirror.as_mut() else {
            return;
        };
        if link.next_sync.is_some_and(|next| now < next) {
            return;
        }
        link.next_sync = Some(now + SYNC_INTERVAL);
        let roots: HashMap<&ProjectId, String> = self
            .sidebar
            .projects
            .iter()
            .filter_map(|project| {
                Some((&project.id, link.canonical_roots.get(&project.repo_root)?))
            })
            .collect();
        let changed: Vec<ThreadUpsert> = self
            .sidebar
            .threads
            .iter()
            .filter_map(|thread| thread_upsert(thread, roots.get(&thread.project_id).cloned()))
            .filter(|upsert| link.mirrored.get(&upsert.thread_id) != Some(upsert))
            .collect();
        // A full queue sends nothing; the same changes are found again next time.
        if !changed.is_empty() && link.handle.try_upsert_threads(changed.clone()) {
            for upsert in changed {
                link.mirrored.insert(upsert.thread_id.clone(), upsert);
            }
        }
    }
}

/// The stored definition of a sidebar thread. `None` for an empty ID, which
/// storage would reject together with the rest of the batch, and for a record
/// whose state is still pending.
fn thread_upsert(thread: &ThreadRecord, project_root: Option<String>) -> Option<ThreadUpsert> {
    let thread_id = thread.id.to_string();
    // A record whose archive and pin state is not settled yet is not told to the
    // database: the neutral values it holds are not final.
    if thread_id.is_empty() || thread.state_pending {
        return None;
    }
    let point = thread
        .lineage
        .as_ref()
        .and_then(|lineage| lineage.point.as_ref());
    // An escalated conversation is shown as a child but has no fork lineage.
    let lineage_kind = if thread.escalation_source_run_id.is_some() {
        Some(ThreadLineageKind::Escalation)
    } else {
        thread.lineage.as_ref().map(|lineage| match lineage.kind {
            LineageKind::Fork => ThreadLineageKind::Fork,
            LineageKind::Rewind => ThreadLineageKind::Rewind,
        })
    };
    Some(ThreadUpsert {
        thread_id,
        project_id: Some(thread.project_id.to_string()),
        project_root,
        title: Some(thread.title.clone()),
        root_run_id: thread.root_run_id.clone(),
        chat_role: thread.chat_role.map(|role| role_name(role).to_owned()),
        parent_thread_id: thread.parent_thread_id.as_ref().map(ToString::to_string),
        lineage_kind,
        fork_point_run_id: point.map(|point| point.run_id.clone()),
        // A length SQLite cannot hold would also reject the whole batch.
        fork_point_context_len: point
            .map(|point| point.context_len)
            .filter(|length| i64::try_from(*length).is_ok()),
        escalation_source_run_id: thread.escalation_source_run_id.clone(),
        archived: Some(thread.archived),
        // A record archived before the time was kept sends none: storage keeps the
        // time unknown (and never invents a newer one) until a numbered edit comes.
        archived_at_ns: thread
            .archived_at
            .map(|at| at.saturating_mul(NANOS_PER_SECOND)),
        pinned: Some(thread.pinned),
        archive_revision: thread.archive_revision,
        pin_revision: thread.pin_revision,
        superseded: Some(thread.superseded),
        // Older records without a creation time send 0, which keeps the stored one.
        created_at_ns: thread.created_at.saturating_mul(NANOS_PER_SECOND),
        updated_at_ns: thread.last_activity().saturating_mul(NANOS_PER_SECOND),
    })
}

/// The runtime's role name, as used in `chat:<Role>:<thread>`.
const fn role_name(role: ThreadChatRole) -> &'static str {
    match role {
        ThreadChatRole::Worker => runtime::Role::Worker,
        ThreadChatRole::Orchestrator => runtime::Role::Orchestrator,
    }
    .name()
}

#[cfg(test)]
mod tests {
    use workspace_ui::{ForkPoint, ThreadId, ThreadLineage};

    use super::*;

    fn record(id: &str) -> ThreadRecord {
        let mut thread = ThreadRecord::new(ThreadId::new(id), ProjectId::new("demo"), "Title");
        thread.created_at = 10;
        thread.updated_at = 0;
        thread
    }

    #[test]
    fn fork_lineage_and_times_are_mirrored() {
        // Given: a forked worker thread that never saw activity.
        let mut thread = record("thread-2");
        thread.chat_role = Some(ThreadChatRole::Worker);
        thread.parent_thread_id = Some(ThreadId::new("thread-1"));
        thread.lineage = Some(ThreadLineage {
            kind: LineageKind::Fork,
            point: Some(ForkPoint {
                run_id: "run-1".into(),
                context_len: 4,
            }),
        });

        // When: it is mapped to a storage definition.
        let upsert = thread_upsert(&thread, Some("/repo".into())).unwrap();

        // Then: the lineage, role name and times follow the runtime's forms.
        assert_eq!(upsert.thread_id, "thread-2");
        assert_eq!(upsert.project_id.as_deref(), Some("demo"));
        assert_eq!(upsert.project_root.as_deref(), Some("/repo"));
        assert_eq!(upsert.chat_role.as_deref(), Some("Worker"));
        assert_eq!(upsert.parent_thread_id.as_deref(), Some("thread-1"));
        assert_eq!(upsert.lineage_kind, Some(ThreadLineageKind::Fork));
        assert_eq!(upsert.fork_point_run_id.as_deref(), Some("run-1"));
        assert_eq!(upsert.fork_point_context_len, Some(4));
        assert_eq!(upsert.escalation_source_run_id, None);
        assert_eq!(upsert.archived, Some(false));
        assert_eq!(upsert.superseded, Some(false));
        assert_eq!(upsert.created_at_ns, 10 * NANOS_PER_SECOND);
        assert_eq!(upsert.updated_at_ns, 10 * NANOS_PER_SECOND);
    }

    #[test]
    fn the_archive_and_pin_series_are_sent_with_their_revisions() {
        // Given: a record archived before times and revisions existed, one archived
        // with a time, and a pinned one.
        let mut legacy = record("legacy");
        legacy.archived = true;
        let mut numbered = record("numbered");
        numbered.set_archived(true, 1_700);
        let mut pinned = record("pinned");
        pinned.set_pin(true);

        // When: they are mapped to storage definitions.
        let legacy = thread_upsert(&legacy, None).unwrap();
        let numbered = thread_upsert(&numbered, None).unwrap();
        let pinned = thread_upsert(&pinned, None).unwrap();

        // Then: the legacy archive is revision 0 with no time (storage dates it by
        // the first mirror, never newer); a numbered edit carries both.
        assert_eq!(
            (
                legacy.archived,
                legacy.archived_at_ns,
                legacy.archive_revision
            ),
            (Some(true), None, 0)
        );
        assert_eq!(
            (
                numbered.archived,
                numbered.archived_at_ns,
                numbered.archive_revision
            ),
            (Some(true), Some(1_700 * NANOS_PER_SECOND), 1)
        );
        assert_eq!(
            (pinned.pinned, pinned.pin_revision, pinned.archive_revision),
            (Some(true), 1, 0)
        );
    }

    #[test]
    fn escalation_outranks_lineage_and_unknowns_stay_unknown() {
        // Given: an escalated orchestrator conversation and a thread with no role yet.
        let mut escalated = record("escalation-run-9");
        escalated.chat_role = Some(ThreadChatRole::Orchestrator);
        escalated.escalation_source_run_id = Some("run-5".into());
        escalated.lineage = Some(ThreadLineage {
            kind: LineageKind::Rewind,
            point: None,
        });
        let fresh = record("thread-3");

        // When: both are mapped without a known project root.
        let escalated = thread_upsert(&escalated, None).unwrap();
        let fresh = thread_upsert(&fresh, None).unwrap();

        // Then: escalation names the lineage, and unknown values are left unset.
        assert_eq!(escalated.lineage_kind, Some(ThreadLineageKind::Escalation));
        assert_eq!(escalated.chat_role.as_deref(), Some("Orchestrator"));
        assert_eq!(escalated.escalation_source_run_id.as_deref(), Some("run-5"));
        assert_eq!(escalated.fork_point_run_id, None);
        assert_eq!(fresh.lineage_kind, None);
        assert_eq!(fresh.chat_role, None);
        assert_eq!(fresh.project_root, None);
        assert_eq!(fresh.root_run_id, None);
    }

    #[test]
    fn definitions_storage_would_reject_are_left_out() {
        // Given: a thread with an empty ID and one with an oversized fork point.
        let empty = record("");
        let mut oversized = record("thread-4");
        oversized.lineage = Some(ThreadLineage {
            kind: LineageKind::Fork,
            point: Some(ForkPoint {
                run_id: "run-1".into(),
                context_len: u64::MAX,
            }),
        });

        // When / Then: the empty ID is skipped and only the length is dropped.
        assert_eq!(thread_upsert(&empty, None), None);
        let oversized = thread_upsert(&oversized, None).unwrap();
        assert_eq!(oversized.fork_point_run_id.as_deref(), Some("run-1"));
        assert_eq!(oversized.fork_point_context_len, None);
    }

    #[cfg(unix)]
    #[test]
    fn project_roots_resolve_like_the_runtime() {
        // Given: a project recorded through a symlink, and one not created yet.
        let directory = tempfile::tempdir().unwrap();
        let real = directory.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = directory.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let pending = directory.path().join("pending");
        let mut roots = CanonicalRoots::default();
        let canonical = |path: &Path| std::fs::canonicalize(path).unwrap();

        // When / Then: the symlink names its target; the missing root stays unknown.
        assert_eq!(
            roots.get(&link).as_deref(),
            Some(canonical(&real).to_string_lossy().as_ref())
        );
        assert_eq!(roots.get(&pending), None);

        // When / Then: once the root exists, the next sync resolves it.
        std::fs::create_dir(&pending).unwrap();
        assert_eq!(
            roots.get(&pending).as_deref(),
            Some(canonical(&pending).to_string_lossy().as_ref())
        );
    }
}
