use gui::dock::{from_dock_state, to_dock_state};
use std::collections::BTreeMap;

use workspace_ui::{
    LayoutNode, Panel, PanelId, PanelKind, Split, SplitDirection, Tabs, Window, Workspace,
};

#[test]
fn workspace_dock_workspace_round_trip_preserves_nested_structure() {
    // Given: the two-level nested default workspace layout
    let workspace = Workspace::default_v01();

    // When: converting to DockState and extracting it again
    let dock = to_dock_state(&workspace).expect("workspace conversion succeeds");
    let extracted = from_dock_state(&dock, &workspace.panels).expect("dock extraction succeeds");

    // Then: the persistence model retains its tree structure and fractions
    assert_eq!(extracted.main.root, workspace.main.root);
}

#[test]
fn extraction_does_not_depend_on_unrendered_rects() {
    // Given: a newly-created DockState whose egui rectangles are Rect::NOTHING
    let workspace = Workspace::default_v01();
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
