//! Pure common comparisons; no second lifecycle state machine or clock.
use super::*;
use workbench_protocol::namespaces::lifecycle::{
    GraphCas,
    owned_graphs::OwnedGraphsCommand,
    quarantine::QuarantineCommand,
    restart::{InterruptBinding, RestartBinding, RestartCommand},
};
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdminInspection {
    Restart(RestartBinding),
    Interrupt(InterruptBinding),
    Graph(GraphCas),
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
                Self::Graph(a),
                AdminIntent::OwnedGraphs(
                    OwnedGraphsCommand::Release(b) | OwnedGraphsCommand::Resume(b),
                ),
            ) => a == &b.binding,
            _ => false,
        }
    }
}
/// Verified adoption is a prerequisite, not inferred from a receipt or a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphOwnership<'a> {
    pub verified_live_adoption: bool,
    pub owner: &'a str,
    pub generation: u64,
    pub resume_committed: bool,
    pub disposal_complete: bool,
}
pub fn is_undisposed(graph: &GraphOwnership<'_>, live_owner: &str, live_generation: u64) -> bool {
    graph.verified_live_adoption
        && graph.owner == live_owner
        && graph.generation == live_generation
        && !graph.resume_committed
        && !graph.disposal_complete
}
