//! Non-executable disposal authority bound to this runtime and a current graph CAS.
use super::*;
use std::collections::{BTreeMap, BTreeSet};
use storage::recovery::graph_control::{GraphControlCas, GraphControlRecord};

pub(super) type DisposalContext = crate::scratch::archive::ScratchAuthority;

/// Limited cleanup authority, never execution authority. A stale/foreign graph
/// or an unproven scratch binding cannot authorize a filesystem mutation.
///
/// ```compile_fail
/// let authority = runtime::DisposalAuthority {};
/// ```
/// ```compile_fail
/// let authority = <runtime::DisposalAuthority as Default>::default();
/// ```
/// ```compile_fail
/// fn execution(authority: runtime::DisposalAuthority) -> runtime::RunConfig {
///     authority.into()
/// }
/// ```
pub struct DisposalAuthority {
    runtime: Weak<Shared>,
    expected: GraphControlCas,
    threads: BTreeSet<String>,
    bound_threads: BTreeSet<String>,
    scratch: Option<Arc<crate::scratch::ThreadScratch>>,
    // Serializes this supervisor's retries. The original epoch is never
    // refreshed after starting a reset, including an unknown writer ACK.
    pub(super) resets: Mutex<BTreeMap<String, u64>>,
}
impl DisposalAuthority {
    pub(crate) fn runtime(&self) -> &Weak<Shared> {
        &self.runtime
    }
    pub(crate) fn expected(&self) -> &GraphControlCas {
        &self.expected
    }
    pub(super) fn threads(&self) -> &BTreeSet<String> {
        &self.threads
    }
    pub(super) fn scratch(&self) -> Option<&Arc<crate::scratch::ThreadScratch>> {
        self.scratch.as_ref()
    }
}
impl AgentRuntime {
    /// Host composition only: retain the existing writer leases, not a new
    /// ownership registry. No client or persisted recipe supplies these paths.
    pub fn set_disposal_context(
        &self,
        leases: Arc<storage::WriterLeases>,
        root: PathBuf,
        sidebar: PathBuf,
    ) {
        let _ = self.shared.disposal_context.set(DisposalContext {
            leases,
            root,
            sidebar,
        });
    }

    /// A complete, current-format CAS is necessary for logical revocation.
    /// Physical cleanup additionally requires the current writer/root binding,
    /// trusted thread bindings and an unused scratch epoch at the point of use.
    pub fn disposal_authority(
        &self,
        expected: GraphControlCas,
    ) -> Result<DisposalAuthority, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        let canonical = store
            .with_database(|db| {
                db.recovery_graph_cas(&expected.restart_id, &expected.manifest.manifest_id)
            })
            .map_err(error)?;
        if canonical != expected
            || !matches!(
                expected.phase,
                storage::recovery::restart::RestartPhase::Adopted
                    | storage::recovery::restart::RestartPhase::Completed
                    | storage::recovery::restart::RestartPhase::Aborted
                    | storage::recovery::restart::RestartPhase::Invalidated
                    | storage::recovery::restart::RestartPhase::Superseded
            )
        {
            return Err(error("disposal graph CAS conflict"));
        }
        let threads = expected
            .membership
            .iter()
            .filter_map(|m| m.thread_id.clone())
            .collect::<BTreeSet<_>>();
        if threads.len() > 64 || threads.iter().any(String::is_empty) {
            return Err(error("invalid disposal thread set"));
        }
        let bound_threads = threads
            .iter()
            .filter(|thread| {
                self.disposal_thread_binding(&expected, thread)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        let scratch = self
            .shared
            .thread_scratch
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        Ok(DisposalAuthority {
            runtime: Arc::downgrade(&self.shared),
            expected,
            threads,
            bound_threads,
            scratch,
            resets: Mutex::new(BTreeMap::new()),
        })
    }

    fn disposal_thread_binding(
        &self,
        expected: &GraphControlCas,
        thread: &str,
    ) -> Result<bool, RuntimeError> {
        let store = self
            .shared
            .run_store
            .get()
            .ok_or_else(|| error("no run store"))?;
        store
            .with_database(|db| {
                let Some(current) = db.thread(thread)? else {
                    return Ok(false);
                };
                let members = expected
                    .membership
                    .iter()
                    .filter(|m| m.thread_id.as_deref() == Some(thread))
                    .collect::<Vec<_>>();
                if members.is_empty()
                    || members
                        .iter()
                        .any(|m| current.root_run_id.as_deref() != Some(m.root_run_id.as_str()))
                {
                    return Ok(false);
                }
                for member in members {
                    let Some(binding) = db.thread_of_run(&member.run_id)? else {
                        return Ok(false);
                    };
                    if binding.thread_id != thread || binding.root_run_id != member.root_run_id {
                        return Ok(false);
                    }
                }
                Ok(true)
            })
            .map_err(error)
    }

    /// Revalidated immediately before a scratch rename. Missing authority and
    /// unreadable bindings retain scratch; they never prevent logical revoke.
    pub(super) fn disposal_scratch_authorized(
        &self,
        authority: &DisposalAuthority,
        control: &GraphControlRecord,
        thread: &str,
    ) -> bool {
        let validate = || -> Result<bool, RuntimeError> {
            self.validate_disposal_runtime(authority)?;
            if !authority.bound_threads.contains(thread) {
                return Ok(false);
            }
            let Some(context) = self.shared.disposal_context.get() else {
                return Ok(false);
            };
            let Some(scratch) = authority.scratch() else {
                return Ok(false);
            };
            if self
                .shared
                .thread_scratch
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .is_none_or(|current| !Arc::ptr_eq(current, scratch))
            {
                return Ok(false);
            }
            let store = self
                .shared
                .run_store
                .get()
                .ok_or_else(|| error("no run store"))?;
            if !context.validate(&store.db_path).map_err(error)? {
                return Ok(false);
            }
            if !store
                .with_database(|db| db.recovery_graph_control(&control.manifest_id))
                .map_err(error)?
                .is_some_and(|current| {
                    current.revoked
                        && current.effect == control.effect
                        && current.revision == control.revision
                })
            {
                return Ok(false);
            }
            self.disposal_thread_binding(authority.expected(), thread)
        };
        match validate() {
            Ok(true) => true,
            Ok(false) => {
                tracing::warn!(%thread, "disposal scratch retained: current authority/binding not proven");
                false
            }
            Err(error) => {
                tracing::warn!(%thread, %error, "disposal scratch retained: authority unavailable");
                false
            }
        }
    }
}
fn error(reason: impl ToString) -> RuntimeError {
    crate::quiesce::runtime::error(reason)
}
