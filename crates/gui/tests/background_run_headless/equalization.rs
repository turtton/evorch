use super::*;
use egui_dock::{Node, NodeIndex};

fn assert_three_equal_panes(h: &HeadlessWorkbench<AgentRuntime>) {
    let tree = h.state().dock().main_surface();
    let mut node = NodeIndex::root().right();
    let mut remaining = 1.0;
    for (run, expected) in [("run-a", 1.0 / 3.0), ("run-c", 0.5)] {
        let Node::Vertical(split) = &tree[node] else {
            panic!("expected vertical split at {node:?}");
        };
        assert_eq!(pane_node(h, run), node.left());
        assert!(
            (split.fraction - expected).abs() < f32::EPSILON,
            "expected {expected}, got {}",
            split.fraction
        );
        assert!((remaining * split.fraction - 1.0 / 3.0).abs() < f32::EPSILON);
        remaining *= 1.0 - split.fraction;
        node = node.right();
    }
    assert_eq!(pane_node(h, "run-d"), node);
    assert!((remaining - 1.0 / 3.0).abs() < f32::EPSILON);
}

#[test]
fn remaining_panes_equalize_when_second_subagent_completes() {
    // Given: four running subagents in insertion order.
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events(["run-a", "run-b", "run-c", "run-d"].map(started));
    // When: the second pane completes and parks on the first leaf.
    h.state_mut().apply_events([completed("run-b")]);
    // Then: the remaining active panes each occupy one third; B remains docked.
    assert_three_equal_panes(&h);
    assert_parked(&h, "run-b");
    h.run();
    assert_three_equal_panes(&h);
}

#[test]
fn remaining_panes_equalize_when_middle_subagent_closes() {
    // Given: four running subagents with closeable agent-run-* IDs.
    let temp = tempfile::tempdir().expect("temp");
    let (mut h, _rt) = workbench(temp.path());
    h.state_mut()
        .apply_events(["run-a", "run-b", "run-c", "run-d"].map(started));
    h.run();
    let path = h
        .state()
        .dock()
        .find_tab(&PanelId::new("agent-run-b"))
        .expect("B");
    // When: egui_dock removes the tab and the workbench reconciles the close.
    h.state_mut().dock_mut().remove_tab(path);
    h.run();
    // Then: each surviving pane occupies one third of the subagent column.
    assert_three_equal_panes(&h);
}
