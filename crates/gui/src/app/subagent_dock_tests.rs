use super::*;

#[test]
fn equalize_sets_chain_fractions_for_n_panes() {
    for count in 1_u16..=6 {
        // Given: a sidebar and an unequal right-hand subagent chain.
        let mut tree = egui_dock::Tree::new(vec![PanelId::new("sidebar")]);
        let [_, first] = tree.split_right(NodeIndex::root(), 0.7, vec![PanelId::new("0")]);
        let mut leaves = vec![first];
        for index in 1..count {
            let bottom = leaves.pop().expect("last pane");
            leaves.extend(tree.split_below(bottom, 0.5, vec![PanelId::new(index.to_string())]));
        }
        // When: insertion equalizes the subagent chain.
        equalize_subagent_fractions(&mut tree, &leaves);
        // Then: fractions encode equal shares without changing sidebar width.
        let Node::Horizontal(sidebar) = &tree[NodeIndex::root()] else {
            panic!("sidebar split");
        };
        assert_eq!(sidebar.fraction, 0.7);
        let mut node = first;
        for remaining in (2..=count).rev() {
            let Node::Vertical(split) = &tree[node] else {
                panic!("subagent split");
            };
            assert!((split.fraction - 1.0 / f32::from(remaining)).abs() < f32::EPSILON);
            node = node.right();
        }
        assert_eq!(Some(&node), leaves.last());
    }
}

#[test]
fn inactive_thread_panes_are_hidden_and_return_with_their_completion_state() {
    let mut state = WorkbenchState::new(
        crate::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    let mut first = workspace_ui::ThreadRecord::new(
        workspace_ui::ThreadId::new("first"),
        workspace_ui::ProjectId::new("project"),
        "First",
    );
    first.run_ids = vec!["run-1".into()];
    let mut second = workspace_ui::ThreadRecord::new(
        workspace_ui::ThreadId::new("second"),
        workspace_ui::ProjectId::new("project"),
        "Second",
    );
    second.run_ids = vec!["run-2".into()];
    state.sidebar.threads = vec![first, second];
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    state.open_subagent_pane("run-1");
    state.open_subagent_pane("run-2");
    state.park_completed_subagent("run-2");
    let id1 = PanelId::new("agent-run-1");
    let id2 = PanelId::new("agent-run-2");
    assert!(state.dock.find_tab(&id1).is_some());
    assert!(state.dock.find_tab(&id2).is_none());
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("second"));
    state.sync_subagent_thread_panes();
    assert!(state.dock.find_tab(&id1).is_none());
    assert!(state.dock.find_tab(&id2).is_some());
    assert!(matches!(
        state.panels[&id2].kind,
        PanelKind::ParkedAgentTranscript(_)
    ));
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    state.sync_subagent_thread_panes();
    assert!(state.dock.find_tab(&id1).is_some());
    assert!(state.dock.find_tab(&id2).is_none());
    // Closing a pane removes its registration during render; switching back
    // only restores panes still registered by that thread.
    let path = state.dock.find_tab(&id1).unwrap();
    state.dock.remove_tab(path);
    state.panels.remove(&id1);
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("second"));
    state.sync_subagent_thread_panes();
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("first"));
    state.sync_subagent_thread_panes();
    assert!(state.dock.find_tab(&id1).is_none());
}

#[test]
fn runs_without_an_owning_thread_do_not_open_right_hand_tabs() {
    let mut state = WorkbenchState::new(
        crate::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    state.open_subagent_pane("run-orphan");
    state.park_completed_subagent("run-orphan");
    state.sync_subagent_thread_panes();
    assert!(
        state
            .dock
            .find_tab(&PanelId::new("agent-run-orphan"))
            .is_none()
    );
}
