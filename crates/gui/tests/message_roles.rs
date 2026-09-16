use event_bus::{Event, MessageEvent};
use gui::model::transcript::TranscriptModel;

struct Source;

impl gui::model::tasks::AgentRunSource for Source {
    fn list(&self) -> Vec<runtime::AgentSummary> {
        [(1, "orchestrator"), (2, "worker")]
            .map(|(id, role)| runtime::AgentSummary {
                run_id: runtime::RunId::new(id),
                name: "custom-name".into(),
                role_name: role.into(),
                phase: event_bus::AgentRunPhase::Running,
                model: "model".into(),
            })
            .to_vec()
    }
}

#[test]
fn conversation_renders_each_speakers_role() {
    // Given: two known speakers in a real workbench conversation.
    let mut state =
        gui::app::WorkbenchState::new(Source, &workspace_ui::UiSettings::default()).unwrap();
    state.apply_events([
        Event::new(MessageEvent::MessageDelta {
            delta: "Plan".into(),
            run_id: Some("run-1".into()),
        }),
        Event::new(MessageEvent::ReasoningDelta {
            delta: "Consider".into(),
            run_id: Some("run-2".into()),
        }),
    ]);
    let mut harness = gui::headless::HeadlessWorkbench::new(state, [1200.0, 900.0]);
    // When: the conversation pane renders.
    harness.run();
    // Then: the message and reasoning each expose their own speaker's role.
    assert!(harness.has_label("[orchestrator]"));
    assert!(harness.has_label("[worker]"));
    if let Some(path) = std::env::var_os("ROLE_BADGE_EVIDENCE") {
        harness
            .capture()
            .unwrap()
            .save_png(std::path::Path::new(&path))
            .unwrap();
    }
}

#[test]
fn role_lookup_uses_the_speakers_task_row() {
    // Given: distinct run identities and roles, not the selected run's role.
    let rows = [(1, "orchestrator"), (2, "worker")].map(|(id, role)| gui::model::tasks::TaskRow {
        run_id: runtime::RunId::new(id),
        name: "custom-name".into(),
        role: role.into(),
        status: event_bus::AgentRunPhase::Running,
        model: "model".into(),
    });
    // When / Then: resolving each speaker returns its role, never a fallback.
    assert_eq!(
        gui::model::tasks::role_for_run(&rows, "run-1"),
        Some("orchestrator")
    );
    assert_eq!(
        gui::model::tasks::role_for_run(&rows, "run-2"),
        Some("worker")
    );
    assert_eq!(gui::model::tasks::role_for_run(&rows, "run-3"), None);
}

#[test]
fn deltas_coalesce_when_run_and_kind_match() {
    for reasoning in [false, true] {
        // Given: a fresh transcript and two fragments from the same run.
        let mut model = TranscriptModel::new();
        // When: both fragments arrive.
        for delta in ["first", "second"] {
            let event = if reasoning {
                MessageEvent::ReasoningDelta {
                    delta: delta.into(),
                    run_id: Some("run-1".into()),
                }
            } else {
                MessageEvent::MessageDelta {
                    delta: delta.into(),
                    run_id: Some("run-1".into()),
                }
            };
            model.apply(&Event::new(event));
        }
        // Then: they occupy one entry.
        assert_eq!(model.entries().len(), 1);
        assert!(matches!(&model.entries()[0],
            gui::model::transcript::TranscriptEntry::Message { text, run_id }
            | gui::model::transcript::TranscriptEntry::Reasoning { text, run_id }
            if text == "firstsecond" && run_id.as_deref() == Some("run-1")));
    }
}

#[test]
fn deltas_split_when_runs_differ() {
    for reasoning in [false, true] {
        // Given: a fresh transcript receiving the same kind from different runs.
        let mut model = TranscriptModel::new();
        // When: the speaker changes.
        for run_id in [Some("run-1"), Some("run-2"), None] {
            let event = if reasoning {
                MessageEvent::ReasoningDelta {
                    delta: "text".into(),
                    run_id: run_id.map(str::to_owned),
                }
            } else {
                MessageEvent::MessageDelta {
                    delta: "text".into(),
                    run_id: run_id.map(str::to_owned),
                }
            };
            model.apply(&Event::new(event));
        }
        // Then: each speaker has its own entry, including unknown provenance.
        assert_eq!(model.entries().len(), 3);
    }
}
