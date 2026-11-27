//! Publish recovered roots without holding CommandHost through activation.
use super::{CommandService, ThreadBinding};
use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex},
};

/// A temporary admission fence, not a binding or execution capability. Dropping
/// it on error, unwind or cancellation unblocks the thread without publishing.
pub struct RootRecoveryGuard {
    pending: Arc<Mutex<BTreeSet<String>>>,
    bindings: Vec<(String, ThreadBinding)>,
}
impl Drop for RootRecoveryGuard {
    fn drop(&mut self) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (thread, _) in &self.bindings {
            pending.remove(thread);
        }
    }
}
impl CommandService {
    pub fn root_recovery_pending(&self, thread: &str) -> bool {
        self.recovering_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(thread)
    }

    /// The core supplies only current-directory roots validated by recovery's
    /// current authority. Refuse a concurrent command's newer root, never replace it.
    pub fn begin_root_recovery(
        &self,
        bindings: Vec<(String, ThreadBinding)>,
    ) -> Result<RootRecoveryGuard, String> {
        let mut pending = self
            .recovering_roots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut threads = BTreeSet::new();
        for (thread, binding) in &bindings {
            if !threads.insert(thread.clone())
                || pending.contains(thread)
                || self
                    .bindings
                    .root_of(thread)
                    .is_some_and(|root| root != binding.root)
            {
                return Err("conversation binding changed during recovery".into());
            }
        }
        pending.extend(threads);
        Ok(RootRecoveryGuard {
            pending: self.recovering_roots.clone(),
            bindings,
        })
    }

    /// Call only after full activation ACK. The caller holds CommandHost for this
    /// short publication; send cannot observe an unfenced gap before the binding.
    pub fn complete_root_recovery(&mut self, guard: RootRecoveryGuard) {
        assert!(Arc::ptr_eq(&guard.pending, &self.recovering_roots));
        for (thread, binding) in &guard.bindings {
            self.bindings.rebind_conversation_root(thread, binding.root);
            self.bindings.chat_runs.insert(thread.clone(), binding.root);
            if let Some(root) = &binding.project_root {
                self.bindings
                    .thread_roots
                    .insert(thread.clone(), root.clone());
            }
            if let Some(project) = &binding.goal_project {
                self.bindings
                    .goal_projects
                    .insert(thread.clone(), project.clone());
            }
        }
        self.publish_lane_bindings();
        drop(guard);
    }
}
