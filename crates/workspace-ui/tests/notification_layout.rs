use workspace_ui::{LayoutNode, PanelId, Tabs, Workspace, from_json, to_json};

#[test]
fn notifications_append_to_global_tasks_leaf_when_missing() {
    let mut workspace = Workspace::default();
    let id = PanelId::new("notifications-main");
    workspace.panels.remove(&id);
    let LayoutNode::Split(root) = &mut workspace.main.root else {
        panic!("split")
    };
    root.fraction = 0.31;
    let LayoutNode::Split(left) = root.first.as_mut() else {
        panic!("split")
    };
    let LayoutNode::Tabs(tabs) = left.second.as_mut() else {
        panic!("tabs")
    };
    tabs.panels.retain(|panel| panel != &id);
    let loaded = from_json(&to_json(&workspace).unwrap()).unwrap();
    let LayoutNode::Split(root) = &loaded.main.root else {
        panic!("split")
    };
    assert_eq!(root.fraction, 0.31);
    let LayoutNode::Split(left) = root.first.as_ref() else {
        panic!("split")
    };
    assert_eq!(
        *left.second,
        LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("tasks-main"), id],
            active: 0
        })
    );
    loaded.validate().unwrap();
}

#[test]
fn legacy_agents_layout_moves_global_panels_below_projects() {
    let mut workspace = Workspace::default();
    let home = PanelId::new("subagents-home");
    let old = PanelId::new("agents-main");
    workspace.panels.remove(&home);
    workspace.panels.insert(
        old.clone(),
        workspace_ui::Panel {
            id: old.clone(),
            kind: workspace_ui::PanelKind::Agents,
            title: "Agents".into(),
            target: None,
        },
    );
    let LayoutNode::Split(root) = &mut workspace.main.root else {
        panic!("split")
    };
    *root.first = LayoutNode::Tabs(Tabs {
        panels: vec![PanelId::new("sidebar-main")],
        active: 0,
    });
    let LayoutNode::Split(content) = root.second.as_mut() else {
        panic!("split")
    };
    let LayoutNode::Split(right) = content.second.as_mut() else {
        panic!("split")
    };
    *right.first = LayoutNode::Tabs(Tabs {
        panels: vec![
            old.clone(),
            PanelId::new("tasks-main"),
            PanelId::new("notifications-main"),
        ],
        active: 0,
    });
    let loaded = from_json(&to_json(&workspace).unwrap()).unwrap();
    loaded.validate().unwrap();
    assert!(!loaded.panels.contains_key(&old));
    assert!(loaded.panels.contains_key(&home));
    let LayoutNode::Split(root) = &loaded.main.root else {
        panic!("split")
    };
    let LayoutNode::Split(left) = root.first.as_ref() else {
        panic!("split")
    };
    let LayoutNode::Tabs(globals) = left.second.as_ref() else {
        panic!("tabs")
    };
    assert!(globals.panels.contains(&PanelId::new("tasks-main")));
    assert!(globals.panels.contains(&PanelId::new("notifications-main")));
    assert_eq!(from_json(&to_json(&loaded).unwrap()).unwrap(), loaded);
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

#[test]
fn retiring_floating_agents_preserves_other_floating_panels() {
    use workspace_ui::{Floating, Panel, PanelKind, Window, WindowRect};
    let mut workspace = Workspace::default();
    let agents = PanelId::new("old-floating-agents");
    let terminal = PanelId::new("floating-terminal");
    for (id, kind) in [
        (agents.clone(), PanelKind::Agents),
        (terminal.clone(), PanelKind::Terminal),
    ] {
        workspace.panels.insert(
            id.clone(),
            Panel {
                id,
                kind,
                title: kind.default_title().into(),
                target: None,
            },
        );
    }
    workspace.extra_windows.push(Window {
        root: LayoutNode::Tabs(Tabs {
            panels: vec![agents.clone()],
            active: 0,
        }),
        floating: vec![Floating {
            node: LayoutNode::Tabs(Tabs {
                panels: vec![terminal.clone()],
                active: 0,
            }),
            rect: WindowRect {
                x: 0.0,
                y: 0.0,
                width: 200.0,
                height: 200.0,
            },
        }],
        rect: None,
    });
    let loaded = from_json(&to_json(&workspace).unwrap()).unwrap();
    loaded.validate().unwrap();
    assert!(!loaded.panels.contains_key(&agents));
    assert_eq!(loaded.extra_windows.len(), 1);
    assert_eq!(
        loaded.extra_windows[0].root,
        LayoutNode::Tabs(Tabs {
            panels: vec![terminal],
            active: 0
        })
    );
}
