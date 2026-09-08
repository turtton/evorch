use workspace_ui::{
    LayoutError, Panel, PanelId, PanelKind, PersistError, Workspace, from_json, to_json,
};

#[test]
fn v2_layout_with_goal_and_merge_panels_migrates_to_v3_pruned() {
    // Given: a frozen snapshot of the previous default, not the new constructor.
    let source = include_str!("fixtures/workspace_v2.json");
    // When: loading through the public persistence boundary.
    let ws = from_json(source).expect("v2 layout migrates");
    // Then: only the surviving default panels remain.
    assert_eq!(ws.version, 3);
    assert_eq!(ws.panels.len(), 5);
    assert_eq!(ws, Workspace::default_v02());
}

#[test]
fn v2_layout_whose_tabs_node_only_held_goal_collapses_split() {
    // Given: a goal-only side of a split and a surviving conversation.
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/workspace_v2.json")).expect("fixture");
    value["main"]["root"] = serde_json::json!({
        "type": "split", "direction": "horizontal", "fraction": 0.3,
        "first": {"type": "tabs", "panels": ["goal-main"], "active": 0},
        "second": {"type": "tabs", "panels": ["agent-main", "merge-main"], "active": 1}
    });
    // When: loading the old split.
    let ws = from_json(&value.to_string()).expect("split migrates");
    // Then: the surviving tabs become the root and active is clamped.
    assert_eq!(ws.version, 3);
    assert_eq!(
        ws.main.root,
        workspace_ui::LayoutNode::Tabs(workspace_ui::Tabs {
            panels: vec![PanelId::new("agent-main")],
            active: 0,
        })
    );
}

#[test]
fn v1_fixture_loads_and_migrates_to_v3() {
    // Given: the frozen JSON emitted by Workspace::default_v01().
    let source = include_str!("fixtures/workspace_v1.json");

    // When: it crosses the public JSON load boundary.
    let workspace = from_json(source).expect("v1 workspace must migrate");

    // Then: the tree and old panels survive under schema v2.
    assert_eq!(workspace.version, 3);
    assert_eq!(workspace.panels.len(), 3);
    assert_eq!(workspace.main, Workspace::default_v01().main);
    assert_eq!(workspace.panels, Workspace::default_v01().panels);
}

#[test]
fn missing_version_is_rejected() {
    // Given: a workspace JSON object without a version.
    let source = r#"{"panels":{},"main":{}}"#;

    // When: it crosses the migration seam.
    let result = from_json(source);

    // Then: migration fails closed before typed deserialization.
    assert!(
        matches!(result, Err(PersistError::Layout(LayoutError::Migration { detail })) if detail.contains("missing"))
    );
}

#[test]
fn v2_floating_and_extra_windows_prune_custom_ids_and_empty_nodes() {
    // Given: removed kinds have custom ids in floating and extra windows.
    let mut value: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/workspace_v2.json")).expect("fixture");
    value["panels"]["old-goal"] =
        serde_json::json!({"id":"old-goal", "kind":"goal", "title":"Custom"});
    value["main"]["floating"] = serde_json::json!([
        {"node":{"type":"tabs","panels":["old-goal"],"active":0},"rect":{"x":0,"y":0,"width":100,"height":100}}
    ]);
    value["extra_windows"] = serde_json::json!([
        {"root":{"type":"tabs","panels":["merge-main"],"active":0},"floating":[]},
        {"root":{"type":"split","direction":"vertical","fraction":0.4,
            "first":{"type":"tabs","panels":["old-goal"],"active":0},
            "second":{"type":"tabs","panels":["terminal-main","merge-main"],"active":1}},"floating":[]}
    ]);
    value["main"]["root"]["second"]["second"]["panels"] =
        serde_json::json!(["agents-main", "diff-main"]);
    // When: loading through JSON migration.
    let ws = from_json(&value.to_string()).expect("windows migrate");
    // Then: empty containers disappear and the extra split collapses.
    assert!(ws.main.floating.is_empty());
    assert_eq!(ws.extra_windows.len(), 1);
    assert_eq!(
        ws.extra_windows[0].root,
        workspace_ui::LayoutNode::Tabs(workspace_ui::Tabs {
            panels: vec![PanelId::new("terminal-main")],
            active: 0,
        })
    );
    assert!(!ws.panels.contains_key(&PanelId::new("old-goal")));
}

#[test]
fn future_version_is_rejected() {
    // Given: a syntactically valid future workspace version.
    let source = r#"{"version":99}"#;

    // When: it crosses the migration seam.
    let result = from_json(source);

    // Then: the unsupported version remains typed.
    assert_eq!(
        result,
        Err(PersistError::Layout(LayoutError::UnsupportedVersion {
            found: 99,
            supported: 3,
        }))
    );
}

#[test]
fn agent_transcript_without_target_is_rejected() {
    // Given: a valid workspace whose agent panel is changed to an unbound transcript.
    let mut workspace = Workspace::default();
    let panel = workspace
        .panels
        .get_mut(&PanelId::new("agent-main"))
        .expect("panel exists");
    panel.kind = PanelKind::AgentTranscript;

    // When: the workspace crosses the save boundary.
    let result = to_json(&workspace);

    // Then: transcript panels require a target.
    assert_eq!(
        result,
        Err(PersistError::Layout(LayoutError::MissingTarget {
            panel_id: "agent-main".to_owned()
        }))
    );
}

#[test]
fn target_on_non_transcript_kind_is_rejected() {
    // Given: a valid workspace with a target on an ordinary agent panel.
    let mut workspace = Workspace::default();
    let panel = workspace
        .panels
        .get_mut(&PanelId::new("agent-main"))
        .expect("panel exists");
    panel.target = Some("run-7".to_owned());

    // When: the workspace crosses the save boundary.
    let result = to_json(&workspace);

    // Then: only transcript panels may carry targets.
    assert_eq!(
        result,
        Err(PersistError::Layout(LayoutError::UnexpectedTarget {
            panel_id: "agent-main".to_owned()
        }))
    );
}

#[test]
fn panel_id_key_mismatch_is_rejected() {
    // Given: a valid workspace with a registry key that differs from Panel.id.
    let mut workspace = Workspace::default();
    workspace.panels.insert(
        PanelId::new("wrong-key"),
        Panel {
            id: PanelId::new("actual-id"),
            kind: PanelKind::Agent,
            title: "Agent".to_owned(),
            target: None,
        },
    );

    // When: the workspace crosses the save boundary.
    let result = to_json(&workspace);

    // Then: registry identity mismatches fail closed.
    assert_eq!(
        result,
        Err(PersistError::Layout(LayoutError::PanelIdMismatch {
            key: "wrong-key".to_owned(),
            panel_id: "actual-id".to_owned(),
        }))
    );
}
