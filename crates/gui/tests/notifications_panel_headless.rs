use egui::{Color32, epaint::Shape};
use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, ToolEvent, UserQuestion};
use gui::{app::WorkbenchState, model::notifications::NotificationsModel, theme::tokens::palette};
use workspace_ui::PanelId;

fn done() -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: "notification-run".into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Done,
        reason: None,
    })
}

fn panel_harness(active: &str) -> Harness<'static, WorkbenchState<gui::fixture::DemoSource>> {
    let mut state = WorkbenchState::new(
        gui::fixture::DemoSource(Vec::new()),
        &workspace_ui::UiSettings::default(),
    )
    .expect("workbench");
    state.notifications_mut().apply_event(&done(), |_| None);
    let path = state.dock().find_tab(&PanelId::new(active)).expect("tab");
    state.dock_mut().set_active_tab(path).expect("activate");
    Harness::builder()
        .with_size(egui::vec2(1280.0, 720.0))
        .build_ui_state(
            |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
            state,
        )
}

#[test]
fn pending_question_button_emits_thread_action_with_run_fallback() {
    use gui::panes::notifications::{NotificationsAction, notifications_pane};
    for (root_name, expected) in [
        (
            "chat:Worker:question-thread",
            NotificationsAction::OpenThread(workspace_ui::ThreadId::new("question-thread")),
        ),
        (
            "legacy-root",
            NotificationsAction::OpenConversation("question-run".into()),
        ),
    ] {
        // Given: a pending user question with a distinct thread and run identity.
        let mut model = NotificationsModel::default();
        model.apply_event(
            &Event::new(ToolEvent::UserQuestionUpdated {
                question: UserQuestion {
                    id: "question-one".into(),
                    run_id: "question-run".into(),
                    root_run_id: "question-run".into(),
                    root_name: root_name.into(),
                    recipient_run_ids: Vec::new(),
                    title: "Which format?".into(),
                    options: vec![],
                    blocking: true,
                    answer: None,
                },
            }),
            |_| None,
        );
        let mut harness = Harness::builder().build_ui_state(
            |ui, (model, action)| {
                if let Some(next) = notifications_pane(ui, model, None) {
                    *action = Some(next);
                }
            },
            (model, None),
        );
        harness.run();
        // When: clicking the explicit navigation button.
        harness.get_by_label("Open thread").click();
        harness.run();
        // Then: the durable thread target wins, with the original run fallback retained.
        assert_eq!(harness.state().1, Some(expected));
    }
}

#[test]
fn question_notification_opens_owning_thread_and_conversation() {
    use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};
    for (root_name, with_run_start) in [
        ("chat:Worker:one", false),
        ("chat:Orchestrator:one", false),
        ("legacy-root", true),
    ] {
        // Given: another active thread and a hidden conversation pane.
        let temp = tempfile::tempdir().unwrap();
        let mut sidebar = SidebarState::default();
        let project = ProjectId::new("project");
        sidebar
            .add_project(project.clone(), "Project", temp.path())
            .unwrap();
        for id in ["one", "two"] {
            sidebar
                .create_thread(ThreadId::new(id), project.clone(), id)
                .unwrap();
        }
        sidebar.switch_thread(&ThreadId::new("two")).unwrap();
        let mut state =
            WorkbenchState::new(gui::fixture::DemoSource(vec![]), &UiSettings::default())
                .unwrap()
                .with_sidebar(sidebar);
        if with_run_start {
            state.apply_events([Event::new(LifecycleEvent::AgentRunStarted {
                run_id: "question-run".into(),
                parent_run_id: None,
                agent_name: "chat:Worker:one".into(),
                role: "worker".into(),
            })]);
        }
        state.apply_events([Event::new(ToolEvent::UserQuestionUpdated {
            question: UserQuestion {
                id: "question-one".into(),
                run_id: "question-run".into(),
                root_run_id: "question-run".into(),
                root_name: root_name.into(),
                recipient_run_ids: Vec::new(),
                title: "Which format?".into(),
                options: vec!["JSON".into()],
                blocking: true,
                answer: None,
            },
        })]);
        state.open_agent_pane("other-run");
        let tab = state
            .dock()
            .find_tab(&PanelId::new("notifications-main"))
            .unwrap();
        state.dock_mut().set_active_tab(tab).unwrap();
        let mut harness = Harness::builder()
            .with_size(egui::vec2(1280.0, 900.0))
            .build_ui_state(
                |ui, state| state.ui(ui, &mut eframe::Frame::_new_kittest()),
                state,
            );
        // The live workbench requests periodic repaints; it need not become idle.
        harness.run_steps(3);
        assert!(harness.query_by_label("Which format?").is_none());
        // When: opening the question's thread from its notification.
        harness.get_by_label("Open thread").click();
        harness.run_steps(3);
        // Then: both sidebar selection and the visible conversation follow the target.
        assert_eq!(
            harness.state().sidebar().active_thread,
            Some(ThreadId::new("one"))
        );
        assert!(harness.query_by_label("Which format?").is_some());
        assert!(harness.query_by_label("回答を送信").is_some());
    }
}

#[test]
fn unread_notification_badge_filled_read_outline() {
    // Given: an unread completed notification.
    let mut model = NotificationsModel::default();
    model.apply_event(&done(), |_| None);
    let mut harness = Harness::builder().build_ui_state(
        |ui, model| {
            gui::panes::notifications::notifications_pane(ui, model, None);
        },
        model,
    );
    // When: rendering before and after acknowledging the displayed revision.
    harness.run();
    assert!(harness.query_by_label("Unread").is_some());
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == palette().SUCCESS
            && rect.rect.height() < gui::theme::tokens::ROW_DENSE)
    }));
    let id = harness.state().items().next().expect("notification").id;
    let revision = harness.state().revision(id).expect("revision");
    harness
        .state_mut()
        .acknowledge(id, Some(&revision), Some(true));
    harness.run_steps(2);
    assert!(harness.query_by_label("Read").is_some());
    // Then: the same badge is outline-only.
    assert!(harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == Color32::TRANSPARENT
&& rect.stroke.color == palette().SUCCESS && rect.stroke.width > 0.0)
    }));
    assert!(!harness.output().shapes.iter().any(|shape| {
        matches!(&shape.shape, Shape::Rect(rect) if rect.fill == palette().SUCCESS)
    }));
}

#[test]
fn panel_ack_clears_unread_when_displayed_and_focused() {
    // Given: the active Notifications panel.
    let mut harness = panel_harness("notifications-main");
    // When: displayed in an explicitly focused viewport.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: its notification is acknowledged.
    assert_eq!(harness.state().notifications().unread_count(), 0);
    harness
        .get_by_label("Run notification-run completed")
        .click();
    harness.run_steps(3);
    assert!(
        harness
            .state()
            .dock()
            .find_tab(&PanelId::new("agent-notification-run"))
            .is_some()
    );
}

#[test]
fn panel_stays_unread_without_outer_focus() {
    for focused in [None, Some(false)] {
        // Given: the active Notifications panel.
        let mut harness = panel_harness("notifications-main");
        // When: displayed without explicit outer focus.
        harness
            .input_mut()
            .viewports
            .entry(egui::ViewportId::ROOT)
            .or_default()
            .focused = focused;
        harness.run_steps(3);
        // Then: it remains unread.
        assert_eq!(harness.state().notifications().unread_count(), 1);
    }
}

#[test]
fn hidden_notifications_tab_stays_unread() {
    // Given: Notifications hidden behind Tasks in the left global leaf.
    let mut harness = panel_harness("tasks-main");
    // When: only the active tab is displayed in a focused viewport.
    harness
        .input_mut()
        .viewports
        .entry(egui::ViewportId::ROOT)
        .or_default()
        .focused = Some(true);
    harness.run_steps(3);
    // Then: the hidden notification remains unread.
    assert_eq!(harness.state().notifications().unread_count(), 1);
}
