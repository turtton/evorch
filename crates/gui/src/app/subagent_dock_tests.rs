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

fn state_with_runs(runs: &[&str]) -> WorkbenchState<crate::fixture::DemoSource> {
    let mut state = WorkbenchState::new(
        crate::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .unwrap();
    let mut thread = workspace_ui::ThreadRecord::new(
        workspace_ui::ThreadId::new("thread"),
        workspace_ui::ProjectId::new("project"),
        "Thread",
    );
    thread.run_ids = runs.iter().map(ToString::to_string).collect();
    state.sidebar.threads = vec![thread];
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("thread"));
    state
}

fn started(run: &str) -> event_bus::Event {
    event_bus::Event::new(event_bus::LifecycleEvent::AgentRunStarted {
        run_id: run.into(),
        parent_run_id: Some("root".into()),
        agent_name: "worker".into(),
        role: "worker".into(),
    })
}

fn changed(run: &str, to: event_bus::AgentRunPhase) -> event_bus::Event {
    event_bus::Event::new(event_bus::LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: event_bus::AgentRunPhase::Pending,
        to,
        reason: None,
    })
}

/// The tab count of the leaf holding `id`; 1 means the pane stands alone.
fn leaf_tab_count(state: &WorkbenchState<crate::fixture::DemoSource>, id: &PanelId) -> usize {
    let path = state.dock.find_tab(id).expect("pane is docked");
    state.dock.leaf(path.node_path()).unwrap().tabs.len()
}

#[test]
fn restarted_run_leaves_the_parked_tab_for_its_own_split() {
    // Given: a finished subagent parked in the region next to a running one.
    let mut state = state_with_runs(&["run-1", "run-2"]);
    state.apply_events([started("run-1"), started("run-2")]);
    state.apply_events([changed("run-1", event_bus::AgentRunPhase::Done)]);
    let id1 = PanelId::new("agent-run-1");
    assert!(matches!(
        state.panels[&id1].kind,
        PanelKind::ParkedAgentTranscript(_)
    ));
    assert!(leaf_tab_count(&state, &id1) > 1);

    // When: the runtime resumes it and announces the start again.
    state.apply_events([started("run-1")]);

    // Then: it is an ordinary transcript pane in a leaf of its own.
    assert_eq!(state.panels[&id1].kind, PanelKind::SubagentTranscript);
    assert_eq!(leaf_tab_count(&state, &id1), 1);
    // Parking again after it finishes works as for any other pane.
    state.apply_events([changed("run-1", event_bus::AgentRunPhase::Done)]);
    assert!(matches!(
        state.panels[&id1].kind,
        PanelKind::ParkedAgentTranscript(_)
    ));
}

#[test]
fn running_transition_alone_unparks_the_pane() {
    for phase in [
        event_bus::AgentRunPhase::Running,
        event_bus::AgentRunPhase::Pending,
    ] {
        let mut state = state_with_runs(&["run-1"]);
        state.apply_events([started("run-1")]);
        state.apply_events([changed("run-1", event_bus::AgentRunPhase::Done)]);
        let id = PanelId::new("agent-run-1");
        assert!(matches!(
            state.panels[&id].kind,
            PanelKind::ParkedAgentTranscript(_)
        ));

        state.apply_events([changed("run-1", phase)]);

        assert_eq!(state.panels[&id].kind, PanelKind::SubagentTranscript);
        assert_eq!(leaf_tab_count(&state, &id), 1);
    }
}

#[test]
fn parked_pane_of_another_thread_is_docked_as_running_when_its_thread_returns() {
    let mut state = state_with_runs(&["run-1"]);
    state.open_subagent_pane("run-1");
    state.park_completed_subagent("run-1");
    let mut other = workspace_ui::ThreadRecord::new(
        workspace_ui::ThreadId::new("other"),
        workspace_ui::ProjectId::new("project"),
        "Other",
    );
    other.run_ids = vec!["run-2".into()];
    state.sidebar.threads.push(other);
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("other"));
    state.sync_subagent_thread_panes();
    let id = PanelId::new("agent-run-1");
    assert!(state.dock.find_tab(&id).is_none());

    // The run resumes while another thread is open.
    state.unpark_subagent("run-1");
    assert!(state.dock.find_tab(&id).is_none());
    state.sidebar.active_thread = Some(workspace_ui::ThreadId::new("thread"));
    state.sync_subagent_thread_panes();

    assert_eq!(state.panels[&id].kind, PanelKind::SubagentTranscript);
    assert_eq!(leaf_tab_count(&state, &id), 1);
}

#[test]
fn unpark_ignores_panes_that_are_not_parked() {
    let mut state = state_with_runs(&["run-1"]);
    state.open_subagent_pane("run-1");
    let id = PanelId::new("agent-run-1");
    let before = state.dock.find_tab(&id);
    state.unpark_subagent("run-1");
    state.unpark_subagent("run-unknown");
    assert_eq!(state.dock.find_tab(&id), before);
}

/// How many dock tabs show `id`, across every surface.
fn tab_occurrences(state: &WorkbenchState<crate::fixture::DemoSource>, id: &PanelId) -> usize {
    state
        .dock
        .iter_all_tabs()
        .filter(|(_, tab)| *tab == id)
        .count()
}

fn assert_workspace_exports(state: &WorkbenchState<crate::fixture::DemoSource>) {
    let workspace = crate::dock::from_dock_state(&state.dock, &state.panels).unwrap();
    workspace_ui::validate(&workspace).unwrap();
}

#[test]
fn unparking_a_tab_that_is_alone_in_its_leaf_keeps_one_valid_independent_pane() {
    for other_running in [false, true] {
        // Given: the region home is gone, so the parked tab is the only tab of its
        // leaf and removing it prunes that leaf.
        let mut state = state_with_runs(&["run-1", "run-2"]);
        state.open_subagent_pane("run-1");
        if other_running {
            state.open_subagent_pane("run-2");
        }
        state.park_completed_subagent("run-1");
        let home = PanelId::new(REGION_HOME);
        let path = state.dock.find_tab(&home).expect("region home");
        state.dock.remove_tab(path);
        let id = PanelId::new("agent-run-1");
        assert_eq!(leaf_tab_count(&state, &id), 1);

        // When: the run resumes.
        state.unpark_subagent("run-1");

        // Then: the pane is shown once, in a leaf of its own, in a layout that
        // still exports to a valid workspace.
        assert_eq!(state.panels[&id].kind, PanelKind::SubagentTranscript);
        assert_eq!(tab_occurrences(&state, &id), 1);
        assert_eq!(leaf_tab_count(&state, &id), 1);
        if other_running {
            let other = PanelId::new("agent-run-2");
            assert_eq!(tab_occurrences(&state, &other), 1);
            assert_eq!(leaf_tab_count(&state, &other), 1);
        }
        assert_workspace_exports(&state);
    }
}

#[test]
fn unparking_from_a_shared_region_keeps_the_layout_exportable() {
    let mut state = state_with_runs(&["run-1", "run-2"]);
    state.open_subagent_pane("run-1");
    state.open_subagent_pane("run-2");
    state.park_completed_subagent("run-1");
    state.park_completed_subagent("run-2");
    state.unpark_subagent("run-1");
    let id = PanelId::new("agent-run-1");
    assert_eq!(tab_occurrences(&state, &id), 1);
    assert_eq!(leaf_tab_count(&state, &id), 1);
    assert_eq!(tab_occurrences(&state, &PanelId::new("agent-run-2")), 1);
    assert_workspace_exports(&state);
}
