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
/// One retained thread expectation and the currently observed row. The hold
/// itself is not minted from these views; the later Q2 integration owns evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetainedOwner<'a> {
    pub retained_identity: &'a str,
    pub retained_generation: u64,
    pub current_identity: Option<&'a str>,
    pub current_generation: Option<u64>,
}
impl RetainedOwner<'_> {
    fn matches(&self, live_owner: &str) -> bool {
        self.retained_identity == live_owner
            && self.current_identity == Some(live_owner)
            && self.current_generation == Some(self.retained_generation)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevocationObservation {
    NotApplied,
    Confirmed,
    Unknown,
}
/// ManualPending and owner-count overflow never make an eligible graph vanish.
/// No owner limit is applied here: an over-limit graph remains a restart blocker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphOwnership<'a> {
    pub verified_live_adoption: bool,
    pub owners: &'a [RetainedOwner<'a>],
    pub resume_committed: bool,
    pub disposal_complete: bool,
    pub revocation: RevocationObservation,
}
pub fn is_undisposed(graph: &GraphOwnership<'_>, live_owner: &str) -> bool {
    graph.verified_live_adoption
        && !graph.owners.is_empty()
        && graph
            .owners
            .iter()
            .all(|o| o.retained_identity == live_owner)
        && !graph.resume_committed
        && !graph.disposal_complete
        && (graph.owners.iter().all(|o| o.matches(live_owner))
            || graph.revocation == RevocationObservation::Unknown)
}
pub fn is_incomplete_disposal(graph: &GraphOwnership<'_>, live_owner: &str) -> bool {
    graph.verified_live_adoption
        && !graph.owners.is_empty()
        && graph
            .owners
            .iter()
            .all(|o| o.retained_identity == live_owner)
        && graph.revocation == RevocationObservation::Confirmed
        && !graph.disposal_complete
        && !graph.owners.iter().all(|o| o.matches(live_owner))
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
