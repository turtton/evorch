//! Sharing `sidebar.json` between GUI processes.
//!
//! Every save is a locked read-merge-write: if another process saved since
//! this one last read or wrote the file, its content is merged with this
//! process's in a three-way merge against the content this process last
//! synchronised (the base), so neither side's edits are lost. What other
//! processes saved reaches this process's state through
//! [`WorkbenchState::import_shared_sidebar`].

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use super::WorkbenchState;
use super::history::{ComposerDraft, HistorySidebar, HistorySidebarRef, UserMessage};
use crate::model::tasks::AgentRunSource;
use crate::shared_state::FileStamp;
use storage::thread_retention::TombstoneSet;

const LOCK_ATTEMPTS: u32 = 3;
const LOCK_RETRY: Duration = Duration::from_millis(5);

/// What this process knows about the shared file.
#[derive(Default)]
pub(super) struct SharedSidebar {
    /// The content last read from or written for this process's own state.
    base: Option<Vec<u8>>,
    /// The file's stamp after that read or write.
    stamp: Option<FileStamp>,
    /// A save merged in changes this process's state does not hold yet.
    import_pending: bool,
    /// A save found the file locked by another process and must be repeated.
    save_deferred: bool,
    /// The file stamp whose content could not be imported. It only stops the import
    /// from being repeated for the same file: a save still reads and merges the file
    /// itself, and refuses to write over it while it cannot.
    import_failed: Option<FileStamp>,
    /// The sidebar as the file holds it after this process's last successful save or
    /// import (or the one it started from): the only state the database mirror projects
    /// from. What this process holds beyond it (a child not saved yet, a merge not taken
    /// in yet) is local and unconfirmed, and is not told to anyone.
    committed: Option<Arc<workspace_ui::SidebarState>>,
}

impl SharedSidebar {
    pub(super) fn set_committed(&mut self, sidebar: workspace_ui::SidebarState) {
        self.committed = Some(Arc::new(sidebar));
    }

    pub(super) fn committed(&self) -> Option<Arc<workspace_ui::SidebarState>> {
        self.committed.clone()
    }

    pub(super) const fn stamp(&self) -> Option<&FileStamp> {
        self.stamp.as_ref()
    }

    pub(super) const fn import_pending(&self) -> bool {
        self.import_pending
    }
}

/// The shared file is being written by another process; the save waits.
#[derive(Debug)]
pub(super) struct SaveDeferred;

impl std::fmt::Display for SaveDeferred {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("the shared sidebar is locked by another process")
    }
}

impl std::error::Error for SaveDeferred {}

/// Holds an exclusive lock on `<file>.lock` until dropped.
struct SidebarLock(#[allow(dead_code)] std::fs::File);

impl SidebarLock {
    /// Saves are not made without the lock: a few short attempts, then the save
    /// is deferred. The UI thread waits at most a few milliseconds.
    fn acquire(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let mut name = path
            .file_name()
            .ok_or("the sidebar path has no file name")?
            .to_os_string();
        name.push(".lock");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(name))?;
        for attempt in 0..LOCK_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) => {
                    if attempt + 1 < LOCK_ATTEMPTS {
                        std::thread::sleep(LOCK_RETRY);
                    }
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
        Err(Box::new(SaveDeferred))
    }
}

fn message_key(message: &UserMessage) -> String {
    if message.id.is_empty() {
        format!(
            "legacy:{}:{:?}:{}",
            message.thread_id, message.at, message.text
        )
    } else {
        message.id.clone()
    }
}

/// The three-way merge, without any thread a confirmed deletion removed: the merge
/// keeps "the other side's edit", which would bring a deleted thread back from a
/// file saved before its deletion.
fn merge_saved(
    base: &HistorySidebar,
    local: &HistorySidebar,
    disk: &HistorySidebar,
    tombstones: &TombstoneSet,
) -> HistorySidebar {
    let mut merged = merge_three_way(base, local, disk);
    super::thread_retention::prune_tombstoned(&mut merged, tombstones);
    merged
}

fn merge_three_way(
    base: &HistorySidebar,
    local: &HistorySidebar,
    disk: &HistorySidebar,
) -> HistorySidebar {
    HistorySidebar {
        sidebar: workspace_ui::SidebarState::merge_three_way(
            &base.sidebar,
            &local.sidebar,
            &disk.sidebar,
        ),
        user_messages: workspace_ui::merge_keyed(
            &base.user_messages,
            &local.user_messages,
            &disk.user_messages,
            message_key,
            |_, local, _| local.clone(),
        ),
        composer_drafts: workspace_ui::merge_keyed(
            &base.composer_drafts,
            &local.composer_drafts,
            &disk.composer_drafts,
            |draft: &ComposerDraft| draft.draft_id.clone(),
            |_, local, _| local.clone(),
        ),
    }
}

/// The outcome of one transaction on the shared sidebar.
struct Commit {
    /// This process's state when the transaction began.
    local: HistorySidebar,
    /// The newest state with the edit applied.
    latest: HistorySidebar,
    /// Threads whose records differ from the file's (this process's own changes).
    own: BTreeSet<String>,
    /// What was written to the file, if anything.
    written: Option<Vec<u8>>,
    stamp: Option<FileStamp>,
}

fn all_thread_ids(history: &HistorySidebar) -> BTreeSet<String> {
    history
        .sidebar
        .threads
        .iter()
        .map(|thread| thread.id.to_string())
        .collect()
}

/// Reads a saved sidebar and derives the per-record cache of its family states. An older
/// file is **not** converted here: the conversion needs the database's numbers and a
/// saved result, so it is made once at start-up while the exclusive locks are held
/// ([`crate::startup`]). A file of the older form met later (written by an older program)
/// carries no family states, so it adds none and cannot overrule this process's.
fn parse(bytes: &[u8]) -> Result<HistorySidebar, serde_json::Error> {
    let mut parsed: HistorySidebar = serde_json::from_slice(bytes)?;
    parsed.sidebar.refresh_derived();
    Ok(parsed)
}

/// The merge of `disk` into `local` (both serialised), as serialised bytes.
fn merged_bytes(
    base: Option<&[u8]>,
    local: &[u8],
    disk: &[u8],
    tombstones: &TombstoneSet,
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let base = base.map(parse).transpose()?.unwrap_or_default();
    let merged = merge_saved(&base, &parse(local)?, &parse(disk)?, tombstones);
    merged.sidebar.validate()?;
    Ok(serde_json::to_vec_pretty(&HistorySidebarRef {
        sidebar: &merged.sidebar,
        user_messages: &merged.user_messages,
        composer_drafts: &merged.composer_drafts,
    })?)
}

fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), Box<dyn std::error::Error>> {
    // One atomic file holds the original, its resolution, and its draft.
    // Never expose half a resend transition after a crash.
    let tmp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| -> Result<(), Box<dyn std::error::Error>> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// The sidebar this process starts from is the base of later merges.
    pub(super) fn shared_sidebar_baseline(&self, bytes: Vec<u8>, stamp: Option<FileStamp>) {
        let mut shared = self.shared_sidebar.borrow_mut();
        shared.base = Some(bytes);
        shared.stamp = stamp;
        shared.import_pending = false;
    }

    /// After a restart's restore took the messages and drafts from the file:
    /// the base keeps the sidebar this process started from, which may be
    /// older than the file, and the stamp is unknown, so the next save or
    /// import merges whatever another process saved in between.
    pub(super) fn shared_sidebar_restored(&self) {
        let mut shared = self.shared_sidebar.borrow_mut();
        let mut base = shared
            .base
            .as_deref()
            .and_then(|bytes| parse(bytes).ok())
            .unwrap_or_default();
        base.user_messages.clone_from(&self.history);
        base.composer_drafts.clone_from(&self.composer_drafts);
        shared.base = serde_json::to_vec_pretty(&HistorySidebarRef {
            sidebar: &base.sidebar,
            user_messages: &base.user_messages,
            composer_drafts: &base.composer_drafts,
        })
        .ok();
        shared.stamp = None;
        shared.import_pending = true;
    }

    /// Repeats a save that found the file locked.
    pub fn flush_deferred_sidebar_save(&mut self) {
        let deferred = self.shared_sidebar.borrow().save_deferred;
        if deferred {
            self.save_sidebar();
        }
    }

    /// Whether a save is waiting for another process to release the file.
    pub fn sidebar_save_deferred(&self) -> bool {
        self.shared_sidebar.borrow().save_deferred
    }

    /// Writes `local` (this process's serialised state) to `path`, merging in
    /// what another process saved since the last synchronisation.
    pub(super) fn shared_sidebar_write(
        &self,
        path: &Path,
        local: Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _lock = match SidebarLock::acquire(path) {
            Ok(lock) => lock,
            Err(error) => {
                if error.is::<SaveDeferred>() {
                    self.shared_sidebar.borrow_mut().save_deferred = true;
                }
                return Err(error);
            }
        };
        let mut shared = self.shared_sidebar.borrow_mut();
        let mut output = None;
        let current = FileStamp::of(path);
        if current.is_some() && (current != shared.stamp || shared.import_pending) {
            let merged = std::fs::read(path).map_err(Into::into).and_then(|disk| {
                merged_bytes(shared.base.as_deref(), &local, &disk, &self.tombstones)
            });
            match merged {
                Ok(merged) if merged != local => output = Some(merged),
                Ok(_) => {}
                // The newest saved state cannot be read or merged, so this process cannot
                // tell what pins and archives other windows made. Writing its own state
                // could undo them (and the next start would trust the result), so
                // nothing is written until the file can be read again.
                Err(error) => {
                    tracing::error!(%error, path = %path.display(), "could not merge the shared sidebar; not saving over it");
                    return Err(error);
                }
            }
        }
        write_atomically(path, output.as_deref().unwrap_or(&local))?;
        // What the file holds now is the committed state, merged-in changes included.
        if let Ok(written) = parse(output.as_deref().unwrap_or(&local)) {
            shared.set_committed(written.sidebar);
        }
        shared.stamp = FileStamp::of(path);
        shared.import_pending = output.is_some();
        shared.save_deferred = false;
        shared.base = Some(local);
        Ok(())
    }

    /// Edits the shared sidebar as one transaction under the save lock, on the
    /// newest state: the file's content merged with this process's. `op` changes
    /// only what the user's action concerns (it numbers the archive and pin series
    /// it touches), so an action taken on a stale view still applies to what other
    /// windows saved meanwhile. The result is written, adopted by this process and
    /// mirrored at once.
    ///
    /// Nothing changes here when the lock cannot be taken, when the file cannot be
    /// read or merged, when `op` fails or when it changes nothing (nothing is
    /// written then).
    ///
    /// # Errors
    /// The file is locked by another process, unreadable or unwritable, or `op` failed.
    pub(super) fn shared_sidebar_transact<T>(
        &mut self,
        op: impl FnOnce(&mut HistorySidebar) -> Result<T, workspace_ui::ThreadError>,
    ) -> Result<T, Box<dyn std::error::Error>> {
        let (output, commit) = self.commit_shared_sidebar(op)?;
        let Some(commit) = commit else {
            return Ok(output);
        };
        let committed = commit
            .written
            .is_some()
            .then(|| commit.latest.sidebar.clone());
        self.adopt_saved(&commit.local, commit.latest, &commit.own);
        if let Some(bytes) = commit.written {
            let mut shared = self.shared_sidebar.borrow_mut();
            if let Some(committed) = committed {
                shared.set_committed(committed);
            }
            shared.stamp = commit.stamp;
            shared.import_pending = false;
            shared.save_deferred = false;
            shared.base = Some(bytes);
        }
        self.force_thread_mirror();
        Ok(output)
    }

    /// Runs `op` on the newest state and writes the result. `None` when `op` changed
    /// nothing; `Commit::written` is `None` without a shared file.
    fn commit_shared_sidebar<T>(
        &self,
        op: impl FnOnce(&mut HistorySidebar) -> Result<T, workspace_ui::ThreadError>,
    ) -> Result<(T, Option<Commit>), Box<dyn std::error::Error>> {
        let local = HistorySidebar {
            sidebar: self.sidebar.clone(),
            user_messages: self.history.clone(),
            composer_drafts: self.composer_drafts.clone(),
        };
        let Some(path) = self.sidebar_path.clone() else {
            // No shared file (a test or a temporary state): there is nothing to merge.
            let mut latest = local.clone();
            let output = op(&mut latest)?;
            super::thread_retention::prune_tombstoned(&mut latest, &self.tombstones);
            let own = all_thread_ids(&latest);
            let commit = Commit {
                local,
                latest,
                own,
                written: None,
                stamp: None,
            };
            return Ok((output, Some(commit)));
        };
        let _lock = SidebarLock::acquire(&path)?;
        let disk = match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        let mut latest = local.clone();
        let mut disk_threads = BTreeMap::new();
        if let Some(bytes) = &disk {
            // An unreadable file is not "no file": the pins and archives in it are
            // unknown, so the edit is refused and the file stays as it is.
            let parsed = parse(bytes)?;
            let base = self
                .shared_sidebar
                .borrow()
                .base
                .as_deref()
                .map(parse)
                .transpose()?
                .unwrap_or_default();
            latest = merge_saved(&base, &local, &parsed, &self.tombstones);
            disk_threads = parsed
                .sidebar
                .threads
                .into_iter()
                .map(|thread| (thread.id.to_string(), thread))
                .collect();
        }
        let merged = latest.clone();
        let output = op(&mut latest)?;
        if latest.sidebar == merged.sidebar
            && latest.user_messages == merged.user_messages
            && latest.composer_drafts == merged.composer_drafts
        {
            // Nothing to write, but the merge may still have brought in what another
            // process saved: take that in too, so a no-op edit does not leave this
            // window on an older view than the one the edit was checked against.
            let stale = merged.sidebar != local.sidebar
                || merged.user_messages != local.user_messages
                || merged.composer_drafts != local.composer_drafts;
            let commit = stale.then(|| Commit {
                local,
                latest: merged,
                own: BTreeSet::new(),
                written: None,
                stamp: None,
            });
            return Ok((output, commit));
        }
        latest.sidebar.validate()?;
        let bytes = serde_json::to_vec_pretty(&HistorySidebarRef {
            sidebar: &latest.sidebar,
            user_messages: &latest.user_messages,
            composer_drafts: &latest.composer_drafts,
        })?;
        write_atomically(&path, &bytes)?;
        #[cfg(test)]
        self.sidebar_write_count
            .set(self.sidebar_write_count.get() + 1);
        let own: BTreeSet<String> = latest
            .sidebar
            .threads
            .iter()
            .filter(|thread| disk_threads.get(&thread.id.to_string()) != Some(*thread))
            .map(|thread| thread.id.to_string())
            .collect();
        Ok((
            output,
            Some(Commit {
                local,
                latest,
                own,
                written: Some(bytes),
                stamp: FileStamp::of(&path),
            }),
        ))
    }

    /// Takes over what other processes saved: merges the shared file into this
    /// process's sidebar, messages and drafts. The active thread, the selected
    /// project and the composer text stay as they are.
    pub(super) fn import_shared_sidebar(&mut self) {
        let Some(path) = self.sidebar_path.clone() else {
            return;
        };
        let stamp = FileStamp::of(&path);
        if stamp.is_some() && self.shared_sidebar.borrow().import_failed == stamp {
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                tracing::warn!(%error, "shared sidebar unreadable");
                return;
            }
        };
        let imported =
            (|| -> Result<(HistorySidebar, HistorySidebar), Box<dyn std::error::Error>> {
                let disk = parse(&bytes)?;
                let base = self
                    .shared_sidebar
                    .borrow()
                    .base
                    .as_deref()
                    .map(parse)
                    .transpose()?
                    .unwrap_or_default();
                let local = HistorySidebar {
                    sidebar: self.sidebar.clone(),
                    user_messages: self.history.clone(),
                    composer_drafts: self.composer_drafts.clone(),
                };
                let merged = merge_saved(&base, &local, &disk, &self.tombstones);
                merged.sidebar.validate()?;
                Ok((local, merged))
            })();
        let (before, merged) = match imported {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "shared sidebar could not be merged; keeping this process's state");
                // The stamp is not taken over: this process has not seen this content,
                // so a later save must not treat the file as already merged.
                self.shared_sidebar.borrow_mut().import_failed = stamp;
                return;
            }
        };
        self.adopt_saved(&before, merged, &BTreeSet::new());
        // The base is what the file held, not what this process now holds:
        // changes of this process that were not saved yet (a deferred save)
        // stay differences from it and are written by the next save.
        let mut shared = self.shared_sidebar.borrow_mut();
        if let Ok(disk) = parse(&bytes) {
            shared.set_committed(disk.sidebar);
        }
        shared.stamp = stamp;
        shared.import_pending = false;
        shared.import_failed = None;
        shared.base = Some(bytes);
    }

    /// Replaces the shared state with `merged` and brings everything derived
    /// from it up to date. `before` is the state it replaces. `own` names the
    /// threads whose changes this process made itself: nobody else mirrored them,
    /// so they stay to be mirrored by this process.
    fn adopt_saved(
        &mut self,
        before: &HistorySidebar,
        merged: HistorySidebar,
        own: &BTreeSet<String>,
    ) {
        let known: BTreeMap<&workspace_ui::ThreadId, &workspace_ui::ThreadRecord> = before
            .sidebar
            .threads
            .iter()
            .map(|thread| (&thread.id, thread))
            .collect();
        let new_threads: Vec<String> = merged
            .sidebar
            .threads
            .iter()
            .filter(|thread| !known.contains_key(&thread.id))
            .map(|thread| thread.id.to_string())
            .collect();
        let bound_before: BTreeSet<&String> = before
            .sidebar
            .threads
            .iter()
            .flat_map(|thread| &thread.run_ids)
            .collect();
        let late_bound: Vec<(String, Vec<String>)> = merged
            .sidebar
            .threads
            .iter()
            .filter_map(|thread| {
                let shown_unbound = thread
                    .run_ids
                    .iter()
                    .any(|run| !bound_before.contains(run) && self.transcripts.run(run).is_some());
                shown_unbound.then(|| (thread.id.to_string(), thread.run_ids.clone()))
            })
            .collect();
        let mut shown: BTreeMap<String, bool> = before
            .user_messages
            .iter()
            .map(|message| (message_key(message), message.shown_as_sent()))
            .collect();
        let appeared: Vec<UserMessage> = merged
            .user_messages
            .iter()
            .filter(|message| {
                shown.remove(&message_key(message)) != Some(true)
                    && message.shown_as_sent()
                    && !late_bound
                        .iter()
                        .any(|(thread, _)| thread == &message.thread_id)
            })
            .cloned()
            .collect();

        self.sidebar = merged.sidebar;
        self.history = merged.user_messages;
        self.composer_drafts = merged.composer_drafts;

        // Runs this process does not know yet are bound to their conversation.
        // Known runs and every conversation's current root keep following the
        // event order: a root changes when its start event is folded, not when
        // a newer sidebar says so, or earlier events would be misattributed.
        let threads: Vec<(String, Vec<String>)> = self
            .sidebar
            .threads
            .iter()
            .map(|thread| (thread.id.to_string(), thread.run_ids.clone()))
            .collect();
        for (thread, runs) in threads {
            for run in &runs {
                if self.transcripts.run_thread(run).is_none() {
                    self.transcripts.bind_run(run, &thread);
                }
            }
        }
        for thread in new_threads.iter().filter(|thread| !own.contains(*thread)) {
            self.seed_imported_branch(thread);
        }
        for message in appeared {
            self.restore_user_message(message);
        }
        if !late_bound.is_empty() {
            self.rehydrate_threads(&late_bound);
        }
        self.rebuild_delivery_rows();
        self.refresh_draft_banner();
        self.sync_project_trust();
    }
}
