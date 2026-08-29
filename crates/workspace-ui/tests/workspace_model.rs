use std::collections::BTreeMap;

use workspace_ui::{
    FloatingPane, InsertPosition, LayoutError, LayoutNode, Panel, PanelId, PanelKind, SaveLayout,
    Split, SplitDirection, Tabs, WORKSPACE_SCHEMA_VERSION, WindowRect, WindowState, Workspace,
    validate,
};

fn panel(id: &str, kind: PanelKind) -> Panel {
    Panel {
        id: PanelId::new(id),
        kind,
        title: kind.default_title().to_owned(),
    }
}

fn workspace_with(root: LayoutNode, panels: BTreeMap<PanelId, Panel>) -> Workspace {
    Workspace {
        version: WORKSPACE_SCHEMA_VERSION,
        panels,
        main: WindowState {
            root,
            floating: Vec::new(),
            rect: None,
        },
        extra_windows: Vec::new(),
    }
}

#[test]
fn default_v01_places_three_panels_in_nested_binary_splits() {
    // Given: the public v0.1 default workspace constructor.
    // When: the framework-independent workspace is built.
    let workspace = Workspace::default_v01();

    // Then: all three pane kinds exist and the nested layout is valid.
    assert_eq!(workspace.panels.len(), 3);
    assert_eq!(
        workspace.panels[&PanelId::new("agent-main")].kind,
        PanelKind::Agent
    );
    assert_eq!(
        workspace.panels[&PanelId::new("terminal-main")].kind,
        PanelKind::Terminal
    );
    assert_eq!(
        workspace.panels[&PanelId::new("tasks-main")].kind,
        PanelKind::Tasks
    );
    assert_eq!(validate(&workspace), Ok(()));

    let LayoutNode::Split(horizontal) = &workspace.main.root else {
        panic!("default root must be a horizontal split");
    };
    assert_eq!(horizontal.direction, SplitDirection::Horizontal);
    assert_eq!(horizontal.fraction, 0.2);
    let LayoutNode::Split(vertical) = horizontal.second.as_ref() else {
        panic!("default right side must be a vertical split");
    };
    assert_eq!(vertical.direction, SplitDirection::Vertical);
    assert_eq!(vertical.fraction, 0.7);
}

#[test]
fn validation_rejects_invalid_fraction_including_nan() {
    // Given: one registered panel in a split with invalid fractions.
    let id = PanelId::new("agent-main");
    let panels = BTreeMap::from([(id.clone(), panel(id.as_str(), PanelKind::Agent))]);
    let make_workspace = |fraction| {
        workspace_with(
            LayoutNode::Split(Split {
                direction: SplitDirection::Horizontal,
                fraction,
                first: Box::new(LayoutNode::Tabs(Tabs {
                    panels: vec![id.clone()],
                    active: 0,
                })),
                second: Box::new(LayoutNode::Tabs(Tabs {
                    panels: vec![id.clone()],
                    active: 0,
                })),
            }),
            panels.clone(),
        )
    };

    // When: validation is run for boundary and non-finite fractions.
    let zero = validate(&make_workspace(0.0));
    let one = validate(&make_workspace(1.0));
    let nan = validate(&make_workspace(f32::NAN));

    // Then: each invalid fraction is rejected before duplicate placement checks.
    assert!(matches!(zero, Err(LayoutError::InvalidFraction { fraction }) if fraction == 0.0));
    assert!(matches!(one, Err(LayoutError::InvalidFraction { fraction }) if fraction == 1.0));
    assert!(matches!(nan, Err(LayoutError::InvalidFraction { fraction }) if fraction.is_nan()));
}

#[test]
fn validation_rejects_unknown_duplicate_empty_active_and_invalid_rect() {
    // Given: a registry with one panel and independently malformed workspaces.
    let id = PanelId::new("agent-main");
    let panels = BTreeMap::from([(id.clone(), panel(id.as_str(), PanelKind::Agent))]);
    let tabs = |ids: Vec<PanelId>, active| {
        LayoutNode::Tabs(Tabs {
            panels: ids,
            active,
        })
    };

    let unknown = workspace_with(tabs(vec![PanelId::new("missing")], 0), panels.clone());
    let duplicate = workspace_with(
        LayoutNode::Split(Split {
            direction: SplitDirection::Vertical,
            fraction: 0.5,
            first: Box::new(tabs(vec![id.clone()], 0)),
            second: Box::new(tabs(vec![id.clone()], 0)),
        }),
        panels.clone(),
    );
    let empty = workspace_with(tabs(Vec::new(), 0), panels.clone());
    let active = workspace_with(tabs(vec![id.clone()], 1), panels.clone());
    let mut invalid_rect = workspace_with(tabs(vec![id], 0), panels);
    invalid_rect.main.floating.push(FloatingPane {
        node: tabs(vec![PanelId::new("agent-main")], 0),
        rect: WindowRect {
            x: 0.0,
            y: 0.0,
            width: 0.0,
            height: 10.0,
        },
    });

    // When: each malformed workspace is validated.
    // Then: the first violated invariant is represented by a typed error.
    assert_eq!(
        validate(&unknown),
        Err(LayoutError::UnknownPanel {
            panel_id: "missing".to_owned()
        })
    );
    assert_eq!(
        validate(&duplicate),
        Err(LayoutError::DuplicatePanel {
            panel_id: "agent-main".to_owned()
        })
    );
    assert_eq!(validate(&empty), Err(LayoutError::EmptyTabs));
    assert_eq!(
        validate(&active),
        Err(LayoutError::ActiveTabOutOfBounds { active: 1, len: 1 })
    );
    assert_eq!(
        validate(&invalid_rect),
        Err(LayoutError::InvalidRect {
            width: 0.0,
            height: 10.0
        })
    );
}

#[test]
fn recursive_layout_serialization_roundtrip_remains_a_tree() {
    // Given: the recursively boxed default layout tree.
    let workspace = Workspace::default_v01();

    // When: serde JSON serializes and deserializes the complete workspace.
    let json = serde_json::to_string(&workspace).expect("workspace serialization must succeed");
    let restored: Workspace =
        serde_json::from_str(&json).expect("workspace deserialization must succeed");

    // Then: ownership recursion remains an equal tree and validates without loops.
    assert_eq!(restored, workspace);
    assert_eq!(validate(&restored), Ok(()));
}

#[test]
fn public_event_and_insert_position_types_roundtrip_through_serde() {
    // Given: the event-bus payload and every insert-position variant.
    let event = SaveLayout;
    let positions = [
        InsertPosition::First,
        InsertPosition::Last,
        InsertPosition::Before(PanelId::new("agent-main")),
        InsertPosition::After(PanelId::new("tasks-main")),
    ];

    // When: the public boundary types round-trip through serde JSON.
    let event_json = serde_json::to_string(&event).expect("event serialization must succeed");
    let restored_event: SaveLayout =
        serde_json::from_str(&event_json).expect("event deserialization must succeed");
    let positions_json =
        serde_json::to_string(&positions).expect("position serialization must succeed");
    let restored_positions: Vec<InsertPosition> =
        serde_json::from_str(&positions_json).expect("position deserialization must succeed");

    // Then: event-bus and layout-editing consumers receive unchanged values.
    assert_eq!(restored_event, event);
    assert_eq!(restored_positions, positions);
}

#[test]
fn duplicate_panel_across_main_floating_and_extra_windows_is_invalid() {
    // Given: one panel placed in main, floating, and an extra window.
    let mut workspace = Workspace::default_v01();
    workspace.main.floating.push(FloatingPane {
        node: LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("agent-main")],
            active: 0,
        }),
        rect: WindowRect {
            x: 10.0,
            y: 10.0,
            width: 100.0,
            height: 100.0,
        },
    });
    workspace.extra_windows.push(WindowState {
        root: LayoutNode::Tabs(Tabs {
            panels: vec![PanelId::new("terminal-main")],
            active: 0,
        }),
        floating: Vec::new(),
        rect: None,
    });

    // When: validation traverses all placement surfaces.
    let result = validate(&workspace);

    // Then: the first cross-surface duplicate is rejected.
    assert_eq!(
        result,
        Err(LayoutError::DuplicatePanel {
            panel_id: "agent-main".to_owned(),
        })
    );
}
