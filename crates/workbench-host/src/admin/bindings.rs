//! Pure common comparisons; no second lifecycle state machine or clock.
use super::*;
use workbench_protocol::namespaces::lifecycle::{
    GraphCas,
    owned_graphs::{OwnedGraphBinding, OwnedGraphsCommand},
    quarantine::QuarantineCommand,
    restart::{InterruptBinding, RestartBinding, RestartCommand},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminInspection {
    Restart(RestartBinding),
    Interrupt(InterruptBinding),
    Graph(GraphCas),
    OwnedGraph(OwnedGraphBinding),
}
impl AdminInspection {
    pub fn matches(&self, intent: &AdminIntent) -> bool {
        match (self, intent) {
            (Self::Restart(a), AdminIntent::Restart(RestartCommand::Request(b))) => a == &b.binding,
            (Self::Interrupt(a), AdminIntent::Restart(RestartCommand::Interrupt(b))) => {
                a == &b.binding
            }
            (Self::Graph(a), AdminIntent::Quarantine(QuarantineCommand::Discard(b))) => {
                a == &b.binding
            }
            (
                Self::OwnedGraph(a),
                AdminIntent::OwnedGraphs(
                    OwnedGraphsCommand::Release(b) | OwnedGraphsCommand::Resume(b),
                ),
            ) => a == &b.binding,
            _ => false,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationObservation {
    NotApplied,
    Confirmed,
    Unknown,
}
/// Persisted facts of one owned graph. ManualPending and thread-count overflow
/// never make an eligible graph vanish: no thread limit is applied here, so an
/// over-limit graph remains a restart blocker. Nothing here is authority.
///
/// ```compile_fail
/// fn not_authority(facts: workbench_host::admin::GraphDisposalFacts) {
///     let _: runtime::DisposalAuthority = facts.into();
/// }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphDisposalFacts {
    /// The graph was adopted by this core's own successor epoch.
    pub verified_live_adoption: bool,
    /// Distinct threads of the graph, including members beyond the displayed 64.
    pub thread_count: usize,
    pub resume_committed: bool,
    pub disposal_complete: bool,
    pub revocation: RevocationObservation,
}
impl GraphDisposalFacts {
    /// A restart blocker: adopted, not resumed, and its durable revocation is
    /// not yet confirmed (or its disposal is complete, which ends it).
    pub fn is_undisposed(&self) -> bool {
        self.verified_live_adoption
            && self.thread_count > 0
            && !self.resume_committed
            && !self.disposal_complete
            && self.revocation != RevocationObservation::Confirmed
    }
    /// Durable revocation is confirmed and only physical cleanup is left. It is
    /// a warning, never a restart blocker: a cleanup that cannot be proven
    /// (shared or unknown scratch users) retains the scratch and must not hold
    /// every later restart.
    pub fn is_incomplete_disposal(&self) -> bool {
        self.verified_live_adoption
            && self.thread_count > 0
            && self.revocation == RevocationObservation::Confirmed
            && !self.disposal_complete
    }
}

/// Snapshot-local alias collision detector. Raw identities stay server-side;
/// callers share this set across Q1/Q2 and discard it after building a snapshot.
#[derive(Default)]
pub struct GraphReferences(
    std::collections::BTreeMap<workbench_protocol::namespaces::lifecycle::AdminId, Vec<String>>,
);
impl GraphReferences {
    pub fn graph_reference(
        &mut self,
        tx: &str,
        capture: &str,
        manifest: &str,
    ) -> Result<workbench_protocol::namespaces::lifecycle::AdminId, AdminFailure> {
        self.reference("graph", &[tx, capture, manifest])
    }
    pub fn thread_reference(
        &mut self,
        tx: &str,
        capture: &str,
        manifest: &str,
        thread: &str,
    ) -> Result<workbench_protocol::namespaces::lifecycle::AdminId, AdminFailure> {
        self.reference("thread", &[tx, capture, manifest, thread])
    }
    fn reference(
        &mut self,
        domain: &str,
        parts: &[&str],
    ) -> Result<workbench_protocol::namespaces::lifecycle::AdminId, AdminFailure> {
        use sha2::{Digest, Sha256};
        let mut hash = Sha256::new();
        let identity: Vec<_> = std::iter::once(domain)
            .chain(parts.iter().copied())
            .map(str::to_owned)
            .collect();
        for part in &identity {
            hash.update((part.len() as u64).to_be_bytes());
            hash.update(part.as_bytes());
        }
        let id = workbench_protocol::namespaces::lifecycle::AdminId::new(format!(
            "{domain}.{:x}",
            hash.finalize()
        ))
        .map_err(|_| conflict())?;
        self.register(id, identity)
    }
    fn register(
        &mut self,
        id: workbench_protocol::namespaces::lifecycle::AdminId,
        identity: Vec<String>,
    ) -> Result<workbench_protocol::namespaces::lifecycle::AdminId, AdminFailure> {
        match self.0.entry(id.clone()) {
            std::collections::btree_map::Entry::Vacant(e) => {
                e.insert(identity);
            }
            std::collections::btree_map::Entry::Occupied(e) if e.get() == &identity => {}
            _ => return Err(conflict()),
        }
        Ok(id)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn aliases_reject_ambiguous_collision_without_overwriting_the_original() {
        let mut refs = GraphReferences::default();
        let id = refs.graph_reference("tx", "cap", "manifest/raw").unwrap();
        assert!(
            refs.register(id.clone(), vec!["different identity".into()])
                .is_err()
        );
        assert_eq!(
            refs.graph_reference("tx", "cap", "manifest/raw").unwrap(),
            id
        );
    }
}
