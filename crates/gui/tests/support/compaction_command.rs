use super::*;
use event_bus::{CompactionEvent, CompactionReason};
use gui::{app::WorkbenchState, fixture::DemoSource, model::transcript::TranscriptEntry};
use workspace_ui::{ProjectId, SidebarState, ThreadId, UiSettings};

fn state(root: &std::path::Path, sink: RuntimeCommandSink) -> WorkbenchState<DemoSource> {
    let mut sidebar = SidebarState::default();
    let project = ProjectId::new("project");
    sidebar
        .add_project(project.clone(), "Project", root)
        .unwrap();
    sidebar.select_project(&project).unwrap();
    sidebar
        .create_thread(ThreadId::new("selected"), project, "Selected")
        .unwrap();
    sidebar.switch_thread(&ThreadId::new("selected")).unwrap();
    WorkbenchState::new(DemoSource(vec![]), &UiSettings::default())
        .unwrap()
        .with_sidebar(sidebar)
        .with_command_sink(Box::new(sink))
}

fn notice(state: &WorkbenchState<DemoSource>, expected: &str) {
    assert!(
        state.transcripts().thread().entries().iter().any(
            |entry| matches!(entry, TranscriptEntry::Notice { text } if text.contains(expected))
        )
    );
    assert!(
        !state
            .transcripts()
            .thread()
            .entries()
            .iter()
            .any(|entry| matches!(entry, TranscriptEntry::UserMessage { .. }))
    );
}

#[test]
fn composer_requests_manual_compaction_only_for_selected_root_without_sending_a_message() {
    for goal_binding in [false, true] {
        let mut fixture = Fixture::new();
        fixture.runtime = fixture
            .runtime
            .clone()
            .with_compaction(config::CompactionConfig {
                context_window_tokens: 1_000_000,
                keep_recent_tokens: 1,
                summarizer: config::SummarizerKind::Structural,
                ..Default::default()
            });
        let selected = fixture.send("selected", &"old request ".repeat(30));
        fixture.wait_for_reply(&selected, "reply-1");
        let other = fixture.send("other", "unrelated conversation");
        fixture.wait_for_reply(&other, "reply-2");
        let run = fixture.run_id(&selected);
        let before = fixture.runtime.inspect_agent(run).unwrap().message_count;
        let invocations = fixture.invocations.lock().unwrap().len();
        let mut fresh =
            RuntimeCommandSink::new(fixture.runtime.clone(), fixture.rt.handle().clone());
        if goal_binding {
            fresh.bind_goal_context("selected", "project", &selected);
            fixture.sink = fresh;
        }
        let replacement =
            RuntimeCommandSink::new(fixture.runtime.clone(), fixture.rt.handle().clone());
        let sink = std::mem::replace(&mut fixture.sink, replacement);
        let mut state = state(fixture._directory.path(), sink);
        state.composer_mut().input = "/compaction".into();
        state.submit_composer();
        assert_eq!(
            state.issued(),
            &[WorkbenchCommand::CompactChat {
                thread_id: "selected".into()
            }]
        );
        notice(
            &state,
            "Context compaction requested for the next turn boundary",
        );
        let inspection = fixture.runtime.inspect_agent(run).unwrap();
        assert_eq!(inspection.phase, AgentRunPhase::Waiting);
        assert_eq!(inspection.message_count, before);
        assert_eq!(fixture.invocations.lock().unwrap().len(), invocations);

        // The command only queues a request. A real follow-up crosses the boundary.
        fixture
            .runtime
            .send_message(run, "next turn".into())
            .unwrap();
        fixture.rt.block_on(async {
            loop {
                if let EventKind::Compaction(CompactionEvent::Compacted {
                    run_id, reason, ..
                }) = fixture.events.recv().await.unwrap().kind
                {
                    assert_eq!(run_id, selected);
                    assert_eq!(reason, CompactionReason::Manual);
                    break;
                }
            }
        });
        assert_eq!(
            fixture
                .runtime
                .inspect_agent(fixture.run_id(&other))
                .unwrap()
                .phase,
            AgentRunPhase::Waiting
        );
    }
}

#[test]
fn missing_conversations_report_notices_and_terminal_compaction_does_not_resume() {
    for terminal in [
        None,
        Some(AgentRunPhase::Stopped),
        Some(AgentRunPhase::Done),
        Some(AgentRunPhase::Error),
    ] {
        let mut fixture = Fixture::new();
        let run = terminal.map(|phase| {
            let run = if phase == AgentRunPhase::Stopped {
                let id = fixture.send("selected", "request");
                fixture.wait_for_reply(&id, "reply-1");
                let run = fixture.run_id(&id);
                fixture
                    .runtime
                    .stop(run, runtime::StopScope::SelfOnly)
                    .unwrap();
                run
            } else {
                let _guard = fixture.rt.enter();
                if phase == AgentRunPhase::Error {
                    fixture.model.responses.lock().unwrap().clear();
                }
                fixture.runtime.delegate_background(
                    Role::Worker,
                    "request".into(),
                    runtime::RunConfig::default(),
                )
            };
            let actual = fixture.rt.block_on(fixture.runtime.wait(run)).unwrap();
            assert_eq!(actual, phase);
            fixture
                .sink
                .bind_goal_context("selected", "project", &run.to_string());
            run
        });
        let count = fixture.runtime.list_agents().len();
        let replacement =
            RuntimeCommandSink::new(fixture.runtime.clone(), fixture.rt.handle().clone());
        let sink = std::mem::replace(&mut fixture.sink, replacement);
        let mut state = state(fixture._directory.path(), sink);
        state.composer_mut().input = "/compaction".into();
        state.submit_composer();
        notice(
            &state,
            if terminal.is_some() {
                "Compacting saved conversation without starting an agent turn"
            } else {
                "No active conversation run to compact"
            },
        );
        assert_eq!(fixture.runtime.list_agents().len(), count);
        if let Some(run) = run {
            assert_eq!(
                fixture.runtime.inspect_agent(run).unwrap().phase,
                terminal.unwrap()
            );
        }
    }
}

#[test]
fn unknown_run_is_reported_and_unowned_threads_are_rejected() {
    let mut fixture = Fixture::new();
    fixture
        .sink
        .bind_goal_context("selected", "project", "run-999999");
    assert!(matches!(fixture.sink.submit(WorkbenchCommand::CompactChat {
        thread_id: "selected".into()
    }).as_slice(), [LoopEvent::ChatNotice { thread_id, text }]
        if thread_id == "selected" && text.starts_with("Could not request context compaction:")));
    let host = Arc::new(
        runtime::ownership::OwnerHost::open(
            fixture._directory.path(),
            Default::default(),
            Arc::new(EventBus::new(64)),
        )
        .unwrap(),
    );
    fixture.sink = fixture.sink.with_ownership(host.clone());
    assert!(matches!(
        fixture
            .sink
            .submit(WorkbenchCommand::CompactChat {
                thread_id: "selected".into()
            })
            .as_slice(),
        [LoopEvent::CommandRejected { .. }]
    ));
    let mut state = state(fixture._directory.path(), fixture.sink).with_ownership(host);
    state.composer_mut().input = "/compaction".into();
    state.submit_composer();
    assert!(state.issued().is_empty());
    assert_eq!(state.composer().input, "/compaction");
    notice(&state, "Read-only attach");
}
