//! Sharing `sidebar.json` between GUI processes.
//!
//! Every save is a locked read-merge-write: if another process saved since
//! this one last read or wrote the file, its content is merged with this
//! process's in a three-way merge against the content this process last
//! synchronised (the base), so neither side's edits are lost. What other
//! processes saved reaches this process's state through
//! [`WorkbenchState::import_shared_sidebar`].

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use super::WorkbenchState;
use super::history::{ComposerDraft, HistorySidebar, HistorySidebarRef, UserMessage};
use crate::model::tasks::AgentRunSource;
use crate::shared_state::FileStamp;

const LOCK_ATTEMPTS: u32 = 20;
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
}

impl SharedSidebar {
    pub(super) const fn stamp(&self) -> Option<&FileStamp> {
        self.stamp.as_ref()
    }

    pub(super) const fn import_pending(&self) -> bool {
        self.import_pending
    }
}

/// Holds an exclusive lock on `<file>.lock` until dropped.
struct SidebarLock(#[allow(dead_code)] std::fs::File);

impl SidebarLock {
    /// A save does not wait forever on a stuck holder: after a few attempts it
    /// proceeds unlocked, accepting a rare lost update over a frozen window.
    fn acquire(path: &Path) -> Option<Self> {
        let mut name = path.file_name()?.to_os_string();
        name.push(".lock");
        let file = match std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path.with_file_name(name))
        {
            Ok(file) => file,
            Err(error) => {
                tracing::warn!(%error, "sidebar lock file unavailable; saving without the lock");
                return None;
            }
        };
        for _ in 0..LOCK_ATTEMPTS {
            match file.try_lock() {
                Ok(()) => return Some(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(LOCK_RETRY),
                Err(std::fs::TryLockError::Error(error)) => {
                    tracing::warn!(%error, "sidebar lock failed; saving without the lock");
                    return None;
                }
            }
        }
        tracing::warn!("sidebar lock is held elsewhere; saving without the lock");
        None
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

fn merge_saved(
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

fn parse(bytes: &[u8]) -> Result<HistorySidebar, serde_json::Error> {
    serde_json::from_slice(bytes)
}

/// The merge of `disk` into `local` (both serialised), as serialised bytes.
fn merged_bytes(
    base: Option<&[u8]>,
    local: &[u8],
    disk: &[u8],
) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let base = base.map(parse).transpose()?.unwrap_or_default();
    let merged = merge_saved(&base, &parse(local)?, &parse(disk)?);
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
    /// Records `bytes` as the shared file's content this process has read.
    pub(super) fn shared_sidebar_baseline(&self, bytes: Vec<u8>, stamp: Option<FileStamp>) {
        let mut shared = self.shared_sidebar.borrow_mut();
        shared.base = Some(bytes);
        shared.stamp = stamp;
        shared.import_pending = false;
    }

    /// Writes `local` (this process's serialised state) to `path`, merging in
    /// what another process saved since the last synchronisation.
    pub(super) fn shared_sidebar_write(
        &self,
        path: &Path,
        local: Vec<u8>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let _lock = SidebarLock::acquire(path);
        let mut shared = self.shared_sidebar.borrow_mut();
        let mut output = None;
        let current = FileStamp::of(path);
        if current.is_some() && (current != shared.stamp || shared.import_pending) {
            let merged = std::fs::read(path)
                .map_err(Into::into)
                .and_then(|disk| merged_bytes(shared.base.as_deref(), &local, &disk));
            match merged {
                Ok(merged) if merged != local => output = Some(merged),
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "could not merge the shared sidebar; saving this process's state only");
                }
            }
        }
        write_atomically(path, output.as_deref().unwrap_or(&local))?;
        shared.stamp = FileStamp::of(path);
        shared.import_pending = output.is_some();
        shared.base = Some(local);
        Ok(())
    }

    /// Takes over what other processes saved: merges the shared file into this
    /// process's sidebar, messages and drafts. The active thread, the selected
    /// project and the composer text stay as they are.
    pub(super) fn import_shared_sidebar(&mut self) {
        let Some(path) = self.sidebar_path.clone() else {
            return;
        };
        let stamp = FileStamp::of(&path);
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
                let merged = merge_saved(&base, &local, &disk);
                merged.sidebar.validate()?;
                Ok((local, merged))
            })();
        let (before, merged) = match imported {
            Ok(pair) => pair,
            Err(error) => {
                tracing::warn!(%error, "shared sidebar could not be merged; keeping this process's state");
                let mut shared = self.shared_sidebar.borrow_mut();
                shared.stamp = stamp;
                shared.import_pending = false;
                return;
            }
        };
        self.adopt_saved(&before, merged);
        let saved = HistorySidebarRef {
            sidebar: &self.sidebar,
            user_messages: &self.history,
            composer_drafts: &self.composer_drafts,
        };
        let base = serde_json::to_vec_pretty(&saved).ok();
        let mut shared = self.shared_sidebar.borrow_mut();
        shared.stamp = stamp;
        shared.import_pending = false;
        shared.base = base;
    }

    /// Replaces the shared state with `merged` and brings everything derived
    /// from it up to date. `before` is the state it replaces.
    fn adopt_saved(&mut self, before: &HistorySidebar, merged: HistorySidebar) {
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
        let changed_threads: Vec<String> = merged
            .sidebar
            .threads
            .iter()
            .filter(|thread| known.get(&thread.id).is_none_or(|old| *old != *thread))
            .map(|thread| thread.id.to_string())
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
                shown.remove(&message_key(message)) != Some(true) && message.shown_as_sent()
            })
            .cloned()
            .collect();

        self.sidebar = merged.sidebar;
        self.history = merged.user_messages;
        self.composer_drafts = merged.composer_drafts;

        let threads: Vec<(String, Vec<String>, Option<String>)> = self
            .sidebar
            .threads
            .iter()
            .map(|thread| {
                (
                    thread.id.to_string(),
                    thread.run_ids.clone(),
                    thread.root_run_id.clone(),
                )
            })
            .collect();
        for (thread, runs, root) in threads {
            for run in &runs {
                self.transcripts.bind_run(run, &thread);
            }
            if let Some(root) = root {
                self.transcripts.bind_thread_root(&thread, &root);
            }
        }
        for thread in &new_threads {
            self.seed_imported_branch(thread);
        }
        for message in appeared {
            self.restore_user_message(message);
        }
        self.rebuild_delivery_rows();
        self.refresh_draft_banner();
        self.sync_project_trust();
        self.note_threads_mirrored(&changed_threads);
    }
}
