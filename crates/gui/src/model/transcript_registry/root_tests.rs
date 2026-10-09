use super::*;
use event_bus::{AgentRunPhase, LifecycleEvent};

fn registry() -> TranscriptRegistry {
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("thread", "root");
    registry.bind_run("child", "thread");
    registry.select_thread(Some("thread".into()));
    registry
}

fn started() -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: "child".into(),
        parent_run_id: Some("root".into()),
        agent_name: "worker".into(),
        role: "worker".into(),
    })
}

#[test]
fn child_run_delta_stays_out_of_thread() {
    // Given: a child belongs to the root's conversation.
    let mut registry = registry();
    // When: the child starts and streams private output and reasoning.
    registry.apply(&started());
    for event in [
        MessageEvent::MessageDelta {
            run_id: Some("child".into()),
            delta: "private output".into(),
        },
        MessageEvent::ReasoningDelta {
            run_id: Some("child".into()),
            delta: "private reasoning".into(),
        },
    ] {
        registry.apply(&Event::new(event));
    }
    // Then: only metadata reaches the thread; the run keeps the complete stream.
    let notice = TranscriptEntry::Notice {
        text: "subagent worker (child) started".into(),
    };
    assert_eq!(registry.thread().entries(), std::slice::from_ref(&notice));
    assert_eq!(
        registry.run("child").expect("child").entries(),
        &[
            notice,
            TranscriptEntry::Message {
                text: "private output".into(),
                run_id: Some("child".into())
            },
            TranscriptEntry::Reasoning {
                text: "private reasoning".into(),
                run_id: Some("child".into())
            },
        ]
    );
}

#[test]
fn terminal_notices_are_metadata_only() {
    for (phase, reason, state) in [
        (AgentRunPhase::Done, None, "completed"),
        (AgentRunPhase::Stopped, None, "stopped (resumable)"),
        (AgentRunPhase::Error, None, "failed"),
        (
            AgentRunPhase::Error,
            Some("private failure output"),
            "failed",
        ),
        (AgentRunPhase::Error, Some("cancelled"), "cancelled"),
    ] {
        // Given: a child has streamed output into its own pane.
        let mut registry = registry();
        registry.apply(&started());
        registry.apply(&Event::new(MessageEvent::MessageDelta {
            run_id: Some("child".into()),
            delta: "private output".into(),
        }));
        // When: any terminal variant is received.
        registry.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
            run_id: "child".into(),
            from: AgentRunPhase::Running,
            to: phase,
            reason: reason.map(str::to_owned),
        }));
        // Then: neither stream nor failure reason leaks into the thread.
        assert_eq!(
            registry.thread().entries(),
            &[
                TranscriptEntry::Notice {
                    text: "subagent worker (child) started".into()
                },
                TranscriptEntry::Notice {
                    text: format!("subagent worker (child) {state}")
                },
            ]
        );
        assert!(
            matches!(&registry.run("child").expect("child").entries()[1],
            TranscriptEntry::Message { text, .. } if text == "private output")
        );
    }
}

#[test]
fn presented_artifacts_reach_the_conversation_as_one_card() {
    // Given: the conversation root presents a capture its child produced.
    let mut registry = registry();
    registry.apply(&started());
    let presentation = event_bus::ArtifactPresentation {
        presentation_id: "presentation-1".into(),
        title: Some("Mocks".into()),
        caption: None,
        artifacts: vec![event_bus::PresentedArtifact {
            artifact_id: "artifact-1".into(),
            title: "Login".into(),
            caption: None,
            media_type: "image/png".into(),
            path: "/store/blobs/a.png".into(),
            byte_len: 3,
            sha256: "a".into(),
        }],
    };
    // When: the presentation event arrives, as during live use or replay.
    registry.apply(&Event::new(ToolEvent::ArtifactsPresented {
        run_id: "root".into(),
        presentation: presentation.clone(),
    }));
    // Then: the user-facing thread shows the card; the child's pane does not.
    let card = TranscriptEntry::Artifacts { presentation };
    assert_eq!(registry.thread().entries().last(), Some(&card));
    assert!(
        !registry
            .run("child")
            .expect("child")
            .entries()
            .contains(&card)
    );
}
