use super::*;
use crate::ownership::Lease;
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ScratchUser {
    ObserverLost { run: RunId, incarnation: String },
    Run { run: RunId, incarnation: String },
    Shell { run: RunId, id: String },
}
#[derive(Debug, Default)]
pub(super) struct Usage {
    epoch: u64,
    users: HashSet<ScratchUser>,
}
impl ThreadScratch {
    pub(crate) fn set_archive_wake(&self, wake: std::sync::mpsc::SyncSender<()>) {
        *self
            .archive_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
    }
    pub(crate) fn record_scratch_lease(&self, thread: &str, lease: &Lease) {
        self.lock().leases.insert(thread.into(), lease.clone());
    }
    pub(crate) fn register_user(&self, key: &ScratchKey, user: ScratchUser) -> io::Result<()> {
        let mut state = self.lock();
        let usage = state.usage.entry(key.clone()).or_default();
        // Retain each key's counter even when its last user leaves. Never wrap.
        usage.epoch = usage
            .epoch
            .checked_add(1)
            .ok_or_else(|| io::Error::other("scratch epoch exhausted"))?;
        usage.users.insert(user);
        Ok(())
    }
    pub(crate) fn register_execution(
        &self,
        key: &ScratchKey,
        run: RunId,
        incarnation: &str,
    ) -> io::Result<()> {
        self.register_user(
            key,
            ScratchUser::Run {
                run,
                incarnation: incarnation.into(),
            },
        )?;
        self.lock()
            .execution_keys
            .insert((run, incarnation.into()), key.clone());
        Ok(())
    }
    pub(crate) fn release_execution(&self, run: RunId, incarnation: &str) {
        let key = self
            .lock()
            .execution_keys
            .get(&(run, incarnation.into()))
            .cloned();
        if let Some(key) = key {
            self.release_user(
                &key,
                &ScratchUser::Run {
                    run,
                    incarnation: incarnation.into(),
                },
            );
        }
    }
    pub(crate) fn sync_shell_users(
        &self,
        key: &ScratchKey,
        run: RunId,
        executor: &tools::ToolExecutor,
    ) {
        for job in executor.shell_job_snapshots(&run.to_string()) {
            let user = ScratchUser::Shell {
                run,
                id: job.identity.id().into(),
            };
            if job.state == tools::ShellJobState::Terminal && job.teardown_confirmed {
                self.release_user(key, &user);
            } else if let Err(error) = self.register_user(key, user) {
                tracing::error!(%error,"scratch shell registration failed");
            }
        }
    }
    pub(crate) fn release_user(&self, key: &ScratchKey, user: &ScratchUser) {
        let removed = self
            .lock()
            .usage
            .get_mut(key)
            .is_some_and(|usage| usage.users.remove(user));
        if removed {
            self.wake_archive();
        }
    }
    fn wake_archive(&self) {
        if let Some(wake) = self
            .archive_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
        {
            let _ = wake.try_send(());
        }
    }
    pub(crate) fn archive_snapshot(&self, thread: &str) -> Option<(u64, Lease)> {
        let state = self.lock();
        let key = ScratchKey::Thread(thread.into());
        let epoch = match state.usage.get(&key) {
            Some(usage) if !usage.users.is_empty() => return None,
            Some(usage) => usage.epoch,
            None => 0,
        };
        Some((epoch, state.leases.get(thread)?.clone()))
    }
    pub(crate) fn detach_archived(
        &self,
        thread: &str,
        epoch: u64,
        lease: &Lease,
    ) -> io::Result<Option<bool>> {
        let mut state = self.lock();
        let key = ScratchKey::Thread(thread.into());
        if state
            .usage
            .get(&key)
            .is_some_and(|u| !u.users.is_empty() || u.epoch != epoch)
            || state.leases.get(thread).is_none_or(|current| {
                current.owner_id != lease.owner_id || current.generation != lease.generation
            })
        {
            return Ok(None);
        }
        let removed = self.root.discard_deferred(&key.name())?;
        if removed {
            let count = state.archived.entry(key.name()).or_default();
            *count = count
                .checked_add(1)
                .expect("archive notice generation exhausted");
        }
        Ok(Some(removed))
    }
    pub(crate) fn wake_trash(&self) -> io::Result<()> {
        self.root.purge_trash()
    }
    pub(crate) fn take_archived_notice(&self, key: &ScratchKey, run: RunId) -> bool {
        let mut state = self.lock();
        let name = key.name();
        let generation = state.archived.get(&name).copied().unwrap_or(0);
        let previous = state.archive_seen.insert(run, (name.clone(), generation));
        generation > 0 && previous.is_none_or(|(seen, count)| seen != name || count < generation)
    }
}
