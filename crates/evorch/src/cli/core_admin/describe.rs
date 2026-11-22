//! The wording of Q1/Q2 on the command line: closed enum values are shown by
//! their wire names.
use workbench_protocol::namespaces::lifecycle::{
    owned_graphs::{ActionAvailability, GraphOperationView, OwnedGraphView, UndisposedGraphs},
    quarantine::QuarantineReply,
};

/// The wire name of a closed enum value (`cold_boot`, `manual_idle`, ...).
fn name<T: serde::Serialize>(value: &T) -> String {
    serde_json::to_value(value)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| "unknown".into())
}

pub(super) fn describe_quarantine(reply: &QuarantineReply) -> Vec<String> {
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
            lines.extend(items.iter().map(describe_quarantine_item));
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

pub(super) fn describe_undisposed(graphs: &UndisposedGraphs) -> Vec<String> {
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

pub(super) fn describe_graph(view: &OwnedGraphView) -> Vec<String> {
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

pub(super) fn describe_graph_operation(view: &GraphOperationView) -> Vec<String> {
    vec![format!(
        "graph {}: step {}, {}, next: {}",
        view.graph_ref.as_str(),
        name(&view.step),
        name(&view.observation),
        name(&view.next_action)
    )]
}
