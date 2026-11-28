use super::*;
#[cfg(test)]
#[path = "usage_tests.rs"]
pub(crate) mod tests;
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ScratchUser {
    ObserverLost { run: RunId, incarnation: String },
    Run { run: RunId, incarnation: String },
    Shell { run: RunId, id: String },
}
#[derive(Debug, Default)]
pub(super) struct Usage {
    pub(super) epoch: u64,
    pub(super) users: HashSet<ScratchUser>,
    shell_identities: HashMap<ScratchUser, tools::ShellJobIdentity>,
}
/// Read-only disposal observation. Unknown must not become a sharing proof.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DisposalScratchUse {
    Unused { epoch: u64 },
    InUse,
    Unknown,
}
impl ThreadScratch {
    pub(crate) fn disposal_usage(&self, thread: &str) -> DisposalScratchUse {
        let state = self.lock();
        let key = ScratchKey::Thread(thread.into());
        let epoch = state.usage.get(&key).map_or(0, |usage| usage.epoch);
        // Exhaustion and a lost shell observer are unknown, even if another
        // known user exists. No proof may be reconstructed from a lease alone.
        if epoch == u64::MAX
            || state.usage.get(&key).is_some_and(|usage| {
                usage
                    .users
                    .iter()
                    .any(|user| matches!(user, ScratchUser::ObserverLost { .. }))
            })
        {
            return DisposalScratchUse::Unknown;
        }
        if state
            .usage
            .get(&key)
            .is_some_and(|usage| !usage.users.is_empty())
        {
            DisposalScratchUse::InUse
        } else {
            DisposalScratchUse::Unused { epoch }
        }
    }
    pub(crate) fn set_archive_wake(&self, wake: std::sync::mpsc::SyncSender<()>) {
        *self
            .archive_wake
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(wake);
    }
    /// Publication may not fail at epoch exhaustion. Saturation permanently
    /// makes unused snapshots unavailable instead of wrapping to an old epoch.
    pub(crate) fn advance_usage_epoch(&self, key: &ScratchKey) {
        let mut state = self.lock();
        let usage = state.usage.entry(key.clone()).or_default();
        usage.epoch = usage.epoch.saturating_add(1);
        drop(state);
        // An archive pass may have consumed its hint before observing the old
        // epoch. Retrying on this wake must not wait for a user release.
        self.wake_archive();
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
            .remove(&(run, incarnation.into()));
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
        // Serialize snapshot and registration with every other snapshot. Otherwise
        // an older Running observation could reinsert a user after finalization
        // observed teardown and released the job's last registry handle.
        let mut state = self.lock();
        let mut released = false;
        let mut seen = HashSet::new();
        for job in executor.shell_job_snapshots(&run.to_string()) {
            let user = ScratchUser::Shell {
                run,
                id: job.identity.id().into(),
            };
            seen.insert(user.clone());
            let usage = state.usage.entry(key.clone()).or_default();
            if job.state == tools::ShellJobState::Terminal && job.teardown_confirmed {
                released |= usage.users.remove(&user);
                usage.shell_identities.remove(&user);
            } else if let Some(next) = usage.epoch.checked_add(1) {
                usage.epoch = next;
                usage.shell_identities.insert(user.clone(), job.identity);
                usage.users.insert(user);
            } else {
                tracing::error!("scratch shell registration failed: epoch exhausted");
            }
        }
        if let Some(usage) = state.usage.get_mut(key) {
            // Synchronous calls and capacity eviction can remove a terminal
            // record before the watch consumer runs. Its opaque identity keeps
            // the supervisor's proof alive; absence alone never releases it.
            usage.shell_identities.retain(|user, identity| {
                let completed = matches!(user, ScratchUser::Shell { run: owner, .. } if *owner == run)
                    && !seen.contains(user)
                    && identity.teardown_confirmed();
                if completed {
                    released |= usage.users.remove(user);
                }
                !completed
            });
        }
        drop(state);
        if released {
            self.wake_archive();
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
    pub(crate) fn archive_snapshot(&self, thread: &str) -> Option<u64> {
        let state = self.lock();
        let key = ScratchKey::Thread(thread.into());
        let epoch = match state.usage.get(&key) {
            Some(usage) if !usage.users.is_empty() || usage.epoch == u64::MAX => return None,
            Some(usage) => usage.epoch,
            None => 0,
        };
        Some(epoch)
    }
    pub(crate) fn detach_archived(&self, thread: &str, epoch: u64) -> io::Result<Option<bool>> {
        let mut state = self.lock();
        let key = ScratchKey::Thread(thread.into());
        if state
            .usage
            .get(&key)
            .is_some_and(|u| !u.users.is_empty() || u.epoch != epoch || u.epoch == u64::MAX)
        {
            return Ok(None);
        }
        let name = key.name();
        let next = state
            .archived
            .get(&name)
            .copied()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| io::Error::other("archive notice generation exhausted"))?;
        let removed = self.root.discard_deferred(&name)?;
        if removed {
            state.archived.insert(name, next);
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
