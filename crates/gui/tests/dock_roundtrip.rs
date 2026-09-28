use gui::dock::{from_dock_state, to_dock_state};
use std::collections::BTreeMap;

use workspace_ui::{
    LayoutNode, Panel, PanelId, PanelKind, Split, SplitDirection, Tabs, Window, Workspace,
};

#[test]
fn workspace_dock_workspace_round_trip_preserves_nested_structure() {
    // Given: the two-level nested default workspace layout
    let workspace = Workspace::default();

    // When: converting to DockState and extracting it again
    let dock = to_dock_state(&workspace).expect("workspace conversion succeeds");
    let extracted = from_dock_state(&dock, &workspace.panels).expect("dock extraction succeeds");

    // Then: the persistence model retains its tree structure and fractions
    assert_eq!(extracted.main.root, workspace.main.root);
}

#[test]
fn persisted_old_right_group_reconciles_global_tabs_and_keeps_terminal_selection() {
    // Given: the old arrangement with Terminal selected and a custom sidebar width
    let tabs = |ids: &[&str], active| {
        LayoutNode::Tabs(Tabs {
            panels: ids.iter().map(|id| PanelId::new(*id)).collect(),
            active,
        })
    };
    let mut workspace = Workspace::default();
    workspace.panels.remove(&PanelId::new("subagents-home"));
    let agents = PanelId::new("agents-main");
    workspace.panels.insert(
        agents.clone(),
        Panel {
            id: agents,
            kind: PanelKind::Agents,
            title: "Agents".into(),
            target: None,
        },
    );
    workspace.main.root = LayoutNode::Split(Split {
        direction: SplitDirection::Horizontal,
        fraction: 0.35,
        first: Box::new(tabs(&["sidebar-main"], 0)),
        second: Box::new(LayoutNode::Split(Split {
            direction: SplitDirection::Horizontal,
            fraction: 0.625,
            first: Box::new(tabs(&["agent-main"], 0)),
            second: Box::new(tabs(
                &[
                    "agents-main",
                    "diff-main",
                    "terminal-main",
                    "notifications-main",
                ],
                2,
            )),
        })),
    });
    // When: the saved layout crosses the actual reconciliation boundary.
    let encoded = workspace_ui::to_json(&workspace).expect("encode layout");
    let restored = workspace_ui::from_json(&encoded).expect("load layout");
    let dock = to_dock_state(&restored).expect("reconciled dock");
    let extracted = from_dock_state(&dock, &restored.panels).expect("extract workspace");

    // Then: global tabs move below Projects, while surviving custom fractions,
    // right-side tab order and the selected Terminal are preserved.
    assert_eq!(extracted.main.root, restored.main.root);
    let LayoutNode::Split(root) = &extracted.main.root else {
        panic!("root split")
    };
    assert_eq!(root.fraction, 0.35);
    let LayoutNode::Split(left) = root.first.as_ref() else {
        panic!("left split")
    };
    assert_eq!(*left.first, tabs(&["sidebar-main"], 0));
    let LayoutNode::Tabs(globals) = left.second.as_ref() else {
        panic!("global tabs")
    };
    assert!(globals.panels.contains(&PanelId::new("tasks-main")));
    assert!(globals.panels.contains(&PanelId::new("notifications-main")));
    assert!(dock.find_tab(&PanelId::new("agents-main")).is_none());
    let terminal = dock
        .find_tab(&PanelId::new("terminal-main"))
        .expect("terminal");
    let leaf = dock.leaf(terminal.node_path()).expect("terminal leaf");
    assert_eq!(
        leaf.tabs,
        vec![PanelId::new("diff-main"), PanelId::new("terminal-main")]
    );
    assert_eq!(leaf.active, terminal.tab);
    assert_eq!(
        workspace_ui::from_json(&workspace_ui::to_json(&restored).unwrap()).unwrap(),
        restored
    );
}

#[test]
fn extraction_does_not_depend_on_unrendered_rects() {
    // Given: a newly-created DockState whose egui rectangles are Rect::NOTHING
    let workspace = Workspace::default();
    let dock = to_dock_state(&workspace).expect("workspace conversion succeeds");

    // When: extracting without rendering the DockArea
    let extracted = from_dock_state(&dock, &workspace.panels).expect("dock extraction succeeds");

    // Then: layout extraction succeeds and contains no renderer rectangles
    assert_eq!(extracted.main.rect, None);
    assert_eq!(extracted.main.root, workspace.main.root);
}

#[test]
fn complex_nested_split_round_trip_preserves_tree_and_active_tabs() {
    // Given: four registered panels in a two-level nested split
    let panel_ids = ["agent", "terminal", "tasks", "logs"]
        .into_iter()
        .map(|id| {
            let panel_id = PanelId::new(id);
            let panel = Panel {
                id: panel_id.clone(),
                kind: PanelKind::Agent,
                title: id.to_owned(),
                target: None,
            };
            (panel_id, panel)
        })
        .collect::<BTreeMap<_, _>>();
    let tabs = |panels: &[&str], active| {
        LayoutNode::Tabs(Tabs {
            panels: panels.iter().map(|id| PanelId::new(*id)).collect(),
            active,
        })
    };
    let workspace = Workspace {
        version: 1,
        panels: panel_ids,
        main: Window {
            root: LayoutNode::Split(Split {
                direction: SplitDirection::Horizontal,
                fraction: 0.25,
                first: Box::new(tabs(&["agent", "logs"], 1)),
                second: Box::new(LayoutNode::Split(Split {
                    direction: SplitDirection::Vertical,
                    fraction: 0.6,
                    first: Box::new(tabs(&["terminal"], 0)),
                    second: Box::new(tabs(&["tasks"], 0)),
                })),
            }),
            floating: Vec::new(),
            rect: None,
        },
        extra_windows: Vec::new(),
    };

    // When: converting twice between the renderer tree and persistence model
    let first_dock = to_dock_state(&workspace).expect("workspace conversion succeeds");
    let first_extracted =
        from_dock_state(&first_dock, &workspace.panels).expect("extraction succeeds");
    let second_dock = to_dock_state(&first_extracted).expect("second conversion succeeds");
    let second_extracted =
        from_dock_state(&second_dock, &workspace.panels).expect("second extraction succeeds");

    // Then: tree directions, fractions, tabs, and active indices remain equal
    assert_eq!(second_extracted.main.root, workspace.main.root);
}
