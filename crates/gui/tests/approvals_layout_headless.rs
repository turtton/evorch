use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, ToolEvent};
use gui::{
    app::WorkbenchState, dock::from_dock_state, fixture::DemoSource,
    model::commands::WorkbenchCommand,
};
use workspace_ui::{LayoutNode, Panel, PanelId, PanelKind, Tabs, UiSettings, Workspace};

fn settings(workspace: Workspace) -> UiSettings {
    let mut settings = UiSettings::default();
    settings.layout.workspace = Some(workspace);
    settings
}

fn legacy_workspace() -> Workspace {
    let mut workspace = Workspace::default_v01();
    workspace.version = Workspace::default().version;
    let id = PanelId::new("notifications-main");
    workspace.panels.insert(
        id.clone(),
        Panel {
            id: id.clone(),
            kind: PanelKind::Notifications,
            title: "Saved notifications".into(),
            target: None,
        },
    );
    workspace.main.root = LayoutNode::Tabs(Tabs {
        panels: vec![
            PanelId::new("agent-main"),
            id,
            PanelId::new("terminal-main"),
            PanelId::new("tasks-main"),
        ],
        active: 3,
    });
    workspace
}

fn with_approval(workspace: &Workspace, neighbor: &str) -> Workspace {
    fn insert(node: &mut LayoutNode, neighbor: &PanelId) {
        match node {
            LayoutNode::Split(split) => {
                insert(&mut split.first, neighbor);
                insert(&mut split.second, neighbor);
            }
            LayoutNode::Tabs(tabs) => {
                if let Some(index) = tabs.panels.iter().position(|id| id == neighbor) {
                    tabs.panels
                        .insert(index + 1, PanelId::new("approvals-main"));
                    if tabs.active > index {
                        tabs.active += 1;
                    }
                }
            }
        }
    }
    let mut expected = workspace.clone();
    let id = PanelId::new("approvals-main");
    expected.panels.insert(
        id.clone(),
        Panel {
            id,
            kind: PanelKind::Approvals,
            title: PanelKind::Approvals.default_title().into(),
            target: None,
        },
    );
    insert(&mut expected.main.root, &PanelId::new(neighbor));
    expected
}

#[test]
fn inserts_only_missing_approval_when_loading_saved_layouts() {
    for (mut workspace, neighbor) in [
        (legacy_workspace(), "notifications-main"),
        (Workspace::default_v01(), "notifications-main"),
        (Workspace::default_v02(), "notifications-main"),
    ] {
        workspace.version = Workspace::default().version;
        if let LayoutNode::Split(split) = &mut workspace.main.root {
            split.fraction = 0.43;
        }
        // Given: 承認タブを含まない保存済み配置と、そのファイル内容。
        let dir = tempfile::tempdir().expect("temp dir");
        let path = dir.path().join("workspace.json");
        workspace_ui::save_to(&workspace, &path).expect("save legacy layout");
        let original = std::fs::read(&path).expect("saved bytes");
        let loaded = workspace_ui::load_from(&path).expect("load legacy layout");
        let expected = with_approval(&loaded, neighbor);

        // When: 旧配置から Workbench を復元する。
        let state = WorkbenchState::new(DemoSource(Vec::new()), &settings(loaded))
            .expect("restore workbench")
            .with_save_path(&path);

        // Then: 選択・順序・split・サイズを保って承認タブだけが増え、元ファイルは不変。
        let actual = from_dock_state(state.dock(), &expected.panels).expect("extract layout");
        assert_eq!(actual, expected);
        assert_eq!(std::fs::read(&path).expect("saved bytes"), original);
    }
}

#[test]
fn uses_first_leaf_when_saved_layout_has_no_notification_or_agents_tab() {
    // Given: 通知も Agents もない移行済み旧配置。
    let mut workspace = Workspace::default_v01();
    workspace.version = Workspace::default().version;
    let expected = with_approval(&workspace, "tasks-main");
    // When: ファイル移行後の設定から復元する。
    let state = WorkbenchState::new(DemoSource(Vec::new()), &settings(workspace))
        .expect("restore workbench");
    // Then: 最初の既存 leaf にだけ追加され、split は不変。
    assert_eq!(
        from_dock_state(state.dock(), &expected.panels).expect("extract layout"),
        expected
    );
}

#[test]
fn preserves_existing_approval_when_reloading_augmented_layout() {
    // Given: 承認タブが既に配置された保存済みレイアウト。
    let workspace = with_approval(&legacy_workspace(), "notifications-main");
    // When: 同じ配置を再度復元する。
    let state = WorkbenchState::new(DemoSource(Vec::new()), &settings(workspace.clone()))
        .expect("restore workbench");
    // Then: 重複挿入や選択変更がない。
    assert_eq!(
        from_dock_state(state.dock(), &workspace.panels).expect("extract layout"),
        workspace
    );
}

#[test]
fn renders_and_decides_multiple_requests_when_legacy_layout_is_loaded() {
    for (label, approved) in [("Approve", true), ("Reject", false)] {
        // Given: 旧配置を読み込んだ Workbench と複数の保留要求。
        let mut state = WorkbenchState::new(DemoSource(Vec::new()), &settings(legacy_workspace()))
            .expect("restore workbench");
        state.apply_events(
            [("run-2:call-1:17", "shell"), ("run-3:call-1:18", "write")].map(
                |(call_id, tool_name)| {
                    Event::new(ToolEvent::ApprovalRequested {
                        call_id: call_id.into(),
                        tool_name: tool_name.into(),
                        input: None,
                    })
                },
            ),
        );
        let path = state
            .dock()
            .find_tab(&PanelId::new("approvals-main"))
            .expect("restored approvals tab");
        state.dock_mut().set_active_tab(path).expect("activate");
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 900.0))
            .build_ui_state(
                |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
                state,
            );
        harness.run_steps(3);
        assert!(harness.query_by_label("shell").is_some());
        assert!(harness.query_by_label("write").is_some());
        assert_eq!(harness.query_all_by_label("Approve").count(), 2);
        assert_eq!(harness.query_all_by_label("Reject").count(), 2);

        // When: 復元後の一覧から2番目の要求を操作する。
        harness
            .query_all_by_label(label)
            .nth(1)
            .expect("second request")
            .click();
        harness.run_steps(3);

        // Then: 選択した要求の exact ID と判断が1回だけ送られる。
        assert_eq!(
            harness.state().issued(),
            &[WorkbenchCommand::DecideToolApproval {
                call_id: "run-3:call-1:18".into(),
                approved,
            }]
        );
    }
}
