use workspace_ui::{
    LayoutNode, PanelId, PanelKind, SplitDirection, Tabs, WORKSPACE_SCHEMA_VERSION, Workspace,
    validate,
};

#[test]
fn default_v02_places_terminal_below_center_and_diff_below_right_tabs() {
    // Given: the current default workspace constructor.
    // When: the framework-independent workspace is built.
    let workspace = Workspace::default_v02();

    // Then: the exact split tree and unchanged panel registry are present.
    assert_eq!(workspace.version, WORKSPACE_SCHEMA_VERSION);
    assert_eq!(workspace.version, 3);
    assert_eq!(workspace.panels.len(), 6);
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
    let LayoutNode::Split(center) = content.first.as_ref() else {
        panic!("center must split conversation above terminal");
    };
    assert_eq!(center.direction, SplitDirection::Vertical);
    assert_eq!(center.fraction, 0.7);
    assert_eq!(
        center.first.as_ref(),
        &LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("agent-main")],
            active: 0,
        })
    );
    assert_eq!(
        *center.second,
        LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("terminal-main")],
            active: 0,
        })
    );
    let LayoutNode::Split(right) = content.second.as_ref() else {
        panic!("right must split agents/notifications above diff");
    };
    assert_eq!(right.direction, SplitDirection::Vertical);
    assert_eq!(right.fraction, 0.5);
    assert_eq!(
        right.first.as_ref(),
        &LayoutNode::Tabs(Tabs {
            panels: vec![
                PanelId::new("agents-main"),
                PanelId::new("notifications-main"),
            ],
            active: 0,
        })
    );
    assert_eq!(
        *right.second,
        LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("diff-main")],
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
    assert_eq!(Workspace::default(), workspace);
    assert_eq!(
        workspace.panels[&PanelId::new("notifications-main")].kind,
        PanelKind::Notifications
    );
    assert_eq!(validate(&workspace), Ok(()));
}

#[test]
fn default_v02_has_no_goal_or_merge_panels() {
    // Given: the default workspace constructor.
    // When: constructing a workspace.
    let ws = Workspace::default_v02();
    // Then: removed surfaces are absent under schema v3.
    assert_eq!(ws.version, 3);
    assert!(!ws.panels.contains_key(&PanelId::new("goal-main")));
    assert!(!ws.panels.contains_key(&PanelId::new("merge-main")));
}
