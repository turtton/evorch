use workspace_ui::{
    LayoutNode, PanelId, PanelKind, SplitDirection, Tabs, WORKSPACE_SCHEMA_VERSION, Workspace,
    validate,
};

#[test]
fn default_v02_places_sidebar_center_and_right_tabs() {
    // Given: the current default workspace constructor.
    // When: the framework-independent workspace is built.
    let workspace = Workspace::default_v02();

    // Then: the exact three-region tree and panel registry are present.
    assert_eq!(workspace.version, WORKSPACE_SCHEMA_VERSION);
    assert_eq!(workspace.panels.len(), 7);
    let LayoutNode::Split(root) = &workspace.main.root else {
        panic!("default root must be a horizontal split");
    };
    assert_eq!(root.direction, SplitDirection::Horizontal);
    assert_eq!(root.fraction, 0.2);
    assert_eq!(
        root.first.as_ref(),
        &LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("sidebar-main")],
            active: 0,
        })
    );
    let LayoutNode::Split(content) = root.second.as_ref() else {
        panic!("default content must be a horizontal split");
    };
    assert_eq!(content.direction, SplitDirection::Horizontal);
    assert_eq!(content.fraction, 0.625);
    assert_eq!(
        content.first.as_ref(),
        &LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("agent-main")],
            active: 0,
        })
    );
    assert_eq!(
        content.second.as_ref(),
        &LayoutNode::Tabs(Tabs {
            panels: vec![
                PanelId::new("agents-main"),
                PanelId::new("diff-main"),
                PanelId::new("terminal-main"),
                PanelId::new("goal-main"),
                PanelId::new("merge-main"),
            ],
            active: 0,
        })
    );
    assert_eq!(
        workspace.panels[&PanelId::new("sidebar-main")].kind,
        PanelKind::Sidebar
    );
    assert_eq!(
        workspace.panels[&PanelId::new("agents-main")].kind,
        PanelKind::Agents
    );
    assert_eq!(
        workspace.panels[&PanelId::new("diff-main")].kind,
        PanelKind::Diff
    );
    assert_eq!(
        workspace.panels[&PanelId::new("goal-main")].kind,
        PanelKind::Goal
    );
    assert_eq!(
        workspace.panels[&PanelId::new("merge-main")].kind,
        PanelKind::MergeApproval
    );
    assert_eq!(Workspace::default(), workspace);
    assert_eq!(validate(&workspace), Ok(()));
}
