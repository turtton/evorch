//! Persistent attachment mounts follow durable history lineage, not project membership.
use std::{collections::BTreeSet, io};

use sandbox::{ThreadReadDirectory, attachments::AttachmentRoot};

use crate::{AgentRuntime, RunId};

impl AgentRuntime {
    pub fn attachment_root(&self) -> io::Result<AttachmentRoot> {
        Ok(self.thread_scratch()?.attachment_root())
    }

    pub(crate) fn attachment_read_directories_for_run(
        &self,
        run: RunId,
    ) -> io::Result<Vec<ThreadReadDirectory>> {
        let Some(thread) = self.thread_of_run(run) else {
            return Ok(Vec::new());
        };
        self.attachment_read_directories(&thread)
    }

    pub fn attachment_read_directories(
        &self,
        thread: &str,
    ) -> io::Result<Vec<ThreadReadDirectory>> {
        self.deny_deleted_thread(thread).map_err(io::Error::other)?;
        let root = self.attachment_root()?;
        let mut result = vec![root.files(thread)?.read_identity()?];
        let mut visited = BTreeSet::from([thread.to_owned()]);
        let mut current = thread.to_owned();
        while let Some(store) = self.shared.run_store.get() {
            let Some(row) = store.thread(&current).map_err(io::Error::other)? else {
                break;
            };
            let parent = match row.parent_thread_id {
                Some(parent) => Some(parent),
                None => row
                    .escalation_source_run_id
                    .as_ref()
                    .map(|source| store.thread_of_run(source))
                    .transpose()
                    .map_err(io::Error::other)?
                    .flatten()
                    .map(|binding| binding.thread_id),
            };
            let Some(parent) = parent else {
                break;
            };
            if !visited.insert(parent.clone()) {
                return Err(io::Error::other("attachment lineage contains a cycle"));
            }
            let existing = root
                .prepare()?
                .existing_directory(&AttachmentRoot::thread_name(&parent))
                .and_then(|directory| directory.existing_directory("files"));
            match existing {
                Ok(files) => result.push(files.read_identity()?),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            current = parent;
        }
        Ok(result)
    }

    /// The same synchronous deletion is used by both archive implementations and
    /// retention. No run completion, scratch reset, or planned handoff calls it.
    pub fn remove_thread_attachments(&self, thread: &str) -> io::Result<()> {
        let root = self.attachment_root()?;
        let _lock = root.lock_thread(thread)?;
        root.remove_thread(thread)
    }
}

/// Locks are held across the archive state save and physical removal. Taking
/// them never waits while a caller holds the sidebar save lock.
pub struct AttachmentArchive {
    root: AttachmentRoot,
    threads: BTreeSet<String>,
    _locks: Vec<std::fs::File>,
}

impl AttachmentArchive {
    pub fn lock(
        root: AttachmentRoot,
        threads: impl IntoIterator<Item = String>,
    ) -> io::Result<Self> {
        let threads: BTreeSet<_> = threads.into_iter().collect();
        let locks = threads
            .iter()
            .map(|thread| root.try_lock_thread(thread))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self {
            root,
            threads,
            _locks: locks,
        })
    }

    pub fn remove(&self) -> io::Result<()> {
        for thread in &self.threads {
            self.root.remove_thread(thread)?;
        }
        Ok(())
    }
}
