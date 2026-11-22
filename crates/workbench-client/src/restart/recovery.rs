//! Q1 / Q2 recovery intents: a quarantined graph discarded, an undisposed graph
//! released or resumed. Like restart, each command is bound to exactly what the
//! person was shown (revision, core epoch, owners) and can only be built from a
//! [`Confirmed`]. The core, not this layer, refuses whatever is no longer true.
use super::{confirmation::Confirmed, intent::IntentError};
use workbench_protocol::{
    commands::Command,
    namespaces::lifecycle::{
        AdminId, LifecycleCommand,
        contracts::{ConfirmedGraph, GraphCas},
        owned_graphs::{
            ActionAvailability, ConfirmedOwnedGraph, GraphOperationView, OwnedGraphView,
            OwnedGraphsCommand, UndisposedGraphs,
        },
        quarantine::{QuarantineCommand, QuarantineDetail, QuarantineReply},
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
                owner_identity: None,
                owner_generation: None,
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

/// The wire name of a closed enum value (`cold_boot`, `manual_idle`, ...).
fn name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

pub fn describe_quarantine(reply: &QuarantineReply) -> Vec<String> {
    match reply {
        QuarantineReply::List {
            items,
            revision,
            truncated,
        } => {
            let mut lines = vec![format!(
                "{} quarantined graph(s), revision {}",
                items.len(),
                revision.get()
            )];
            lines.extend(items.iter().map(|item| describe_quarantine_item(item)));
            if *truncated {
                lines.push("  (the list was cut off)".into());
            }
            lines
        }
        QuarantineReply::Detail(detail) => {
            let mut lines = vec![format!("core epoch {}", detail.core_epoch.get())];
            lines.push(describe_quarantine_item(&detail.item));
            lines
        }
    }
}

fn describe_quarantine_item(
    item: &workbench_protocol::namespaces::lifecycle::quarantine::QuarantineItem,
) -> String {
    format!(
        "  {} (revision {}): {} member(s), reason {}, eligibility {}, claim {}, discard {}",
        item.graph_id.as_str(),
        item.revision.get(),
        item.members,
        name(&item.reason),
        name(&item.eligibility),
        name(&item.claim),
        name(&item.completion),
    )
}

pub fn describe_undisposed(graphs: &UndisposedGraphs) -> Vec<String> {
    let mut lines = vec![format!(
        "{} undisposed graph(s), snapshot revision {}",
        graphs.count.get(),
        graphs.snapshot_revision.get()
    )];
    for graph in &graphs.references {
        lines.push(format!(
            "  {} (restart {}, revision {})",
            graph.graph_id.as_str(),
            graph.restart_tx_id.as_str(),
            graph.revision.get()
        ));
    }
    if graphs.truncated {
        lines.push("  (the list was cut off)".into());
    }
    if graphs.over_limit_count.get() > 0 {
        lines.push(format!(
            "  {} of them have too many owners to act on here",
            graphs.over_limit_count.get()
        ));
    }
    if graphs.incomplete_disposals.count.get() > 0 {
        lines.push(format!(
            "  {} disposal(s) did not finish and are listed above or in `evorch core quarantine list`",
            graphs.incomplete_disposals.count.get()
        ));
    }
    lines
}

fn availability(action: &ActionAvailability) -> String {
    match action {
        ActionAvailability::Available => "available".into(),
        ActionAvailability::Blocked { problem, detail } => {
            format!("blocked ({}: {})", name(problem), name(detail))
        }
    }
}

pub fn describe_graph(view: &OwnedGraphView) -> Vec<String> {
    vec![
        format!(
            "graph {} (restart {}, snapshot revision {}, core epoch {})",
            view.graph_ref.as_str(),
            view.binding.graph.restart_tx_id.as_str(),
            view.snapshot_revision.get(),
            view.binding.graph.expected_core_epoch.get()
        ),
        format!(
            "  restart {}, disposition {}, claim {}, ownership {}, control {}, eligibility {}",
            name(&view.restart_phase),
            name(&view.disposition),
            name(&view.claim),
            name(&view.ownership),
            name(&view.control),
            name(&view.eligibility),
        ),
        format!("  {} owner thread(s)", view.owner_count.get()),
        format!("  resume: {}", availability(&view.resume)),
        format!("  release: {}", availability(&view.release)),
    ]
}

pub fn describe_graph_operation(view: &GraphOperationView) -> Vec<String> {
    vec![format!(
        "graph {}: step {}, {}, next: {}",
        view.graph_ref.as_str(),
        name(&view.step),
        name(&view.observation),
        name(&view.next_action)
    )]
}
