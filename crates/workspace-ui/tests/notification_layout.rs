use workspace_ui::{LayoutNode, PanelId, Tabs, Workspace, from_json, to_json};

#[test]
fn notifications_append_to_custom_agents_leaf_when_current_layout_lacks_panel() {
    // Given: a current-schema saved layout with reordered tabs and a selected Diff.
    let mut legacy = Workspace::default();
    legacy.panels.remove(&PanelId::new("notifications-main"));
    let LayoutNode::Split(root) = &mut legacy.main.root else {
        panic!("split")
    };
    root.fraction = 0.31;
    let LayoutNode::Split(content) = root.second.as_mut() else {
        panic!("split")
    };
    content.second = Box::new(LayoutNode::Tabs(Tabs {
        panels: vec![
            PanelId::new("terminal-main"),
            PanelId::new("diff-main"),
            PanelId::new("agents-main"),
        ],
        active: 1,
    }));
    let source = to_json(&legacy).unwrap();
    let mut expected = legacy.main.clone();
    let LayoutNode::Split(root) = &mut expected.root else {
        panic!("split")
    };
    let LayoutNode::Split(content) = root.second.as_mut() else {
        panic!("split")
    };
    let LayoutNode::Tabs(tabs) = content.second.as_mut() else {
        panic!("tabs")
    };
    tabs.panels.push(PanelId::new("notifications-main"));
    // When: the saved layout is loaded.
    let loaded = from_json(&source).unwrap();
    // Then: only the appended tab and registry entry differ.
    assert_eq!(loaded.main, expected);
    assert!(
        loaded
            .panels
            .contains_key(&PanelId::new("notifications-main"))
    );
    for (id, panel) in legacy.panels {
        assert_eq!(loaded.panels.get(&id), Some(&panel));
    }
}

#[test]
fn layout_is_unchanged_when_notifications_already_exists() {
    // Given: notifications is already present in a customized layout.
    let mut workspace = Workspace::default();
    let LayoutNode::Split(root) = &mut workspace.main.root else {
        panic!("split")
    };
    root.fraction = 0.31;
    let source = to_json(&workspace).unwrap();
    // When: the layout is loaded twice.
    let loaded = from_json(&source).unwrap();
    let reloaded = from_json(&to_json(&loaded).unwrap()).unwrap();
    // Then: no duplication or arrangement changes occur.
    assert_eq!(loaded, workspace);
    assert_eq!(reloaded, workspace);
}
