//! Q1 / Q2 recovery intents: a quarantined graph discarded, an undisposed graph
//! released or resumed. Like restart, each command is bound to exactly what the
//! person was shown (revision, core epoch) and can only be built from a
//! [`Confirmed`]. The core, not this layer, refuses whatever is no longer true.
use super::{confirmation::Confirmed, intent::IntentError};
use workbench_protocol::{
    commands::Command,
    namespaces::lifecycle::{
        AdminId, LifecycleCommand,
        contracts::{ConfirmedGraph, GraphCas},
        owned_graphs::{
            ActionAvailability, ConfirmedOwnedGraph, OwnedGraphView, OwnedGraphsCommand,
        },
        quarantine::{QuarantineCommand, QuarantineDetail},
    },
    wire::InstanceEpoch,
};

/// Discard the quarantined graph `detail` describes.
pub fn discard_quarantined(
    detail: &QuarantineDetail,
    operation_id: AdminId,
    _confirmed: Confirmed,
) -> Command {
    Command::Lifecycle(LifecycleCommand::Quarantine(QuarantineCommand::Discard(
        ConfirmedGraph {
            binding: GraphCas {
                graph_id: detail.item.graph_id.clone(),
                restart_tx_id: detail.item.restart_tx_id.clone(),
                capture_id: detail.item.capture_id.clone(),
                expected_revision: detail.item.revision,
                expected_core_epoch: detail.core_epoch,
                operation_id,
            },
            confirmed: true,
        },
    )))
}

/// What a person may do with an undisposed graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphAction {
    Release,
    Resume,
}

/// The binding of `view` taken untouched (the view's own operation id is a
/// placeholder and is replaced), after checking the core still offers `action`
/// on exactly this graph. A blocked action is never sent.
pub fn dispose_graph(
    view: &OwnedGraphView,
    action: GraphAction,
    operation_id: AdminId,
    core_epoch: InstanceEpoch,
    _confirmed: Confirmed,
) -> Result<Command, IntentError> {
    let offered = match action {
        GraphAction::Release => &view.release,
        GraphAction::Resume => &view.resume,
    };
    if !matches!(offered, ActionAvailability::Available) {
        return Err(IntentError::Stale("the offered action"));
    }
    if view.binding.graph.expected_core_epoch != core_epoch {
        return Err(IntentError::Stale("core epoch"));
    }
    let mut binding = view.binding.clone();
    binding.graph.operation_id = operation_id;
    let confirmed = ConfirmedOwnedGraph {
        binding,
        confirmed: true,
    };
    Ok(Command::Lifecycle(LifecycleCommand::OwnedGraphs(
        match action {
            GraphAction::Release => OwnedGraphsCommand::Release(confirmed),
            GraphAction::Resume => OwnedGraphsCommand::Resume(confirmed),
        },
    )))
}
