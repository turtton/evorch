use event_bus::{AgentRunPhase, Event, FaultEvent, LifecycleEvent, MessageEvent};
use gui::model::transcript::TranscriptEntry;
use gui::model::transcript_registry::TranscriptRegistry;

fn lag(subscriber_id: u64, skipped: u64) -> Event {
    Event::new(FaultEvent::SubscriberLagged {
        subscriber_id,
        skipped,
    })
}

fn delta(text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        run_id: Some("root".into()),
        delta: text.into(),
    })
}

fn terminal(run: &str, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
    })
}

fn summary(text: &str) -> TranscriptEntry {
    TranscriptEntry::Notice { text: text.into() }
}

#[test]
fn lag_episodes_never_interrupt_stream_or_add_boundary_notices() {
    // Given: root deltas interleaved with lag on an inactive owning thread.
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("owner", "root");
    registry.bind_run("child", "owner");
    registry.select_thread(Some("other".into()));
    for event in [delta("hel"), lag(2, 3), delta("lo"), lag(2, 5), delta("!")] {
        registry.apply(&event);
    }
    // When: a child terminal event provides a thread boundary.
    registry.apply(&terminal("child", AgentRunPhase::Done));
    // Then: only the owner receives the intact message and legitimate boundary.
    assert!(registry.thread().entries().is_empty());
    registry.select_thread(Some("owner".into()));
    assert_eq!(
        registry.thread().entries(),
        &[
            TranscriptEntry::Message {
                text: "hello!".into(),
                run_id: Some("root".into())
            },
            summary("subagent unknown (child) completed"),
        ]
    );
    assert_eq!(
        registry.run("root").unwrap().entries(),
        &[TranscriptEntry::Message {
            text: "hello!".into(),
            run_id: Some("root".into())
        },]
    );
}

#[test]
fn multiple_subscribers_never_add_terminal_summaries() {
    // Given: lag for two independent subscribers.
    for phase in [AgentRunPhase::Done, AgentRunPhase::Error] {
        let mut registry = TranscriptRegistry::new();
        registry.apply(&lag(2, 4));
        registry.apply(&lag(7, 9));
        registry.apply(&lag(2, 6));
        // When: a child terminates without a reason or a preceding start.
        registry.apply(&terminal("child", phase));
        // Then: only the legitimate terminal notice appears.
        let state = match phase {
            AgentRunPhase::Done => "completed",
            AgentRunPhase::Error => "failed",
            _ => unreachable!("fixture only includes terminal phases"),
        };
        assert_eq!(
            registry.thread().entries(),
            &[summary(&format!("subagent unknown (child) {state}")),]
        );
    }
}

#[test]
fn repeated_boundaries_never_flush_lag_entries() {
    // Given: a lag episode followed by a terminal boundary.
    let mut registry = TranscriptRegistry::new();
    registry.apply(&lag(2, 8));
    registry.apply(&terminal("first", AgentRunPhase::Done));
    // When: another lag episode and two more boundaries arrive.
    for event in [
        lag(2, 3),
        terminal("second", AgentRunPhase::Done),
        terminal("third", AgentRunPhase::Done),
    ] {
        registry.apply(&event);
    }
    // Then: every boundary remains free of lag entries.
    assert_eq!(
        registry.thread().entries(),
        &[
            summary("subagent unknown (first) completed"),
            summary("subagent unknown (second) completed"),
            summary("subagent unknown (third) completed"),
        ]
    );
}

#[test]
fn lag_from_multiple_subscribers_leaves_reasoning_uninterrupted() {
    // Given: lag and a bound root whose Done event renders no entry.
    let mut registry = TranscriptRegistry::new();
    registry.bind_thread_root("owner", "root");
    registry.select_thread(Some("owner".into()));
    registry.apply(&lag(2, 5));
    // When: non-entries, run-only deltas, and reasoning arrive before a real boundary.
    for event in [
        terminal("root", AgentRunPhase::Done),
        Event::new(MessageEvent::MessageDelta {
            run_id: Some("child".into()),
            delta: "hidden".into(),
        }),
        Event::new(MessageEvent::ReasoningDelta {
            run_id: Some("root".into()),
            delta: "thinking".into(),
        }),
        lag(7, 2),
        Event::new(MessageEvent::ReasoningDelta {
            run_id: Some("root".into()),
            delta: " more".into(),
        }),
    ] {
        registry.apply(&event);
    }
    // Then: diagnostic-only lag leaves one intact reasoning block.
    assert_eq!(
        registry.thread().entries(),
        &[TranscriptEntry::Reasoning {
            text: "thinking more".into(),
            run_id: Some("root".into())
        },]
    );
}

#[test]
fn lag_never_adds_entries_between_tool_updates_and_agent_messages() {
    use event_bus::{
        AgentMessage, AgentMessageEvent, AgentMessageKind, DeliveryDisposition, ToolEvent,
    };
    // Given: an existing tool entry followed by lag.
    let mut registry = TranscriptRegistry::new();
    let started = Event::new(ToolEvent::ToolStarted {
        tool_name: "read".into(),
        call_id: "call".into(),
        input: None,
        run_id: None,
    });
    registry.apply(&started);
    registry.apply(&lag(2, 4));
    // When: a tool update and run-only delivery precede another entry-producing tool start.
    registry.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "call".into(),
        output: None,
        detail: None,
        is_error: false,
        run_id: None,
    }));
    registry.apply(&Event::new(AgentMessageEvent::Delivered {
        message: AgentMessage {
            message_id: "message".into(),
            sender_run_id: "sender".into(),
            recipient_run_id: "recipient".into(),
            kind: AgentMessageKind::Send,
            content: "hello".into(),
            reply_to: None,
        },
        disposition: DeliveryDisposition::Aside,
    }));
    registry.apply(&lag(2, 6));
    registry.apply(&started);
    // Then: only tool entries appear, even with a reused call ID.
    assert!(matches!(
        registry.thread().entries(),
        [
            TranscriptEntry::Tool {
                status: gui::model::transcript::ToolStatus::Succeeded,
                ..
            },
            TranscriptEntry::Tool {
                status: gui::model::transcript::ToolStatus::Running,
                ..
            },
        ]
    ));
    for run in ["sender", "recipient"] {
        assert!(matches!(
            registry.run(run).unwrap().entries(),
            [TranscriptEntry::AgentMessage { .. }]
        ));
    }
}
