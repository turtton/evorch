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
fn lag_episodes_coalesce_into_single_boundary_notice() {
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
    // Then: only the resolved owner receives one summary, between message and boundary.
    assert!(registry.thread().entries().is_empty());
    registry.select_thread(Some("owner".into()));
    assert_eq!(
        registry.thread().entries(),
        &[
            TranscriptEntry::Message {
                text: "hello!".into(),
                run_id: Some("root".into())
            },
            summary("Subscriber 2 skipped 8 events across 2 lag episodes"),
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
fn lag_summary_precedes_terminal_notice() {
    // Given: lag for two independent subscribers.
    for phase in [AgentRunPhase::Done, AgentRunPhase::Error] {
        let mut registry = TranscriptRegistry::new();
        registry.apply(&lag(2, 4));
        registry.apply(&lag(7, 9));
        registry.apply(&lag(2, 6));
        // When: a child terminates without a reason or a preceding start.
        registry.apply(&terminal("child", phase));
        // Then: subscriber totals remain separate and directly precede the terminal notice.
        let state = match phase {
            AgentRunPhase::Done => "completed",
            AgentRunPhase::Error => "failed",
            _ => unreachable!("fixture only includes terminal phases"),
        };
        assert_eq!(
            registry.thread().entries(),
            &[
                summary("Subscriber 2 skipped 10 events across 2 lag episodes"),
                summary("Subscriber 7 skipped 9 events across 1 lag episodes"),
                summary(&format!("subagent unknown (child) {state}")),
            ]
        );
    }
}

#[test]
fn post_flush_lag_starts_new_episode() {
    // Given: one completed flush.
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
    // Then: counts restart and an empty accumulator adds nothing at the third boundary.
    assert_eq!(
        registry.thread().entries(),
        &[
            summary("Subscriber 2 skipped 8 events across 1 lag episodes"),
            summary("subagent unknown (first) completed"),
            summary("Subscriber 2 skipped 3 events across 1 lag episodes"),
            summary("subagent unknown (second) completed"),
            summary("subagent unknown (third) completed"),
        ]
    );
}

#[test]
fn lag_waits_through_non_entries_and_run_only_events() {
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
        lag(2, 2),
        Event::new(MessageEvent::ReasoningDelta {
            run_id: Some("root".into()),
            delta: " more".into(),
        }),
    ] {
        registry.apply(&event);
    }
    // Then: quiet residue stays pending and reasoning is not split.
    assert_eq!(
        registry.thread().entries(),
        &[TranscriptEntry::Reasoning {
            text: "thinking more".into(),
            run_id: Some("root".into())
        },]
    );
}

#[test]
fn lag_waits_through_agent_messages_and_tool_updates() {
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
    // Then: updates/delivery do not flush; the next actual entry does, even with a reused call ID.
    assert!(matches!(registry.thread().entries(), [
        TranscriptEntry::Tool { status: gui::model::transcript::ToolStatus::Succeeded, .. },
        TranscriptEntry::Notice { text },
        TranscriptEntry::Tool { status: gui::model::transcript::ToolStatus::Running, .. },
    ] if text == "Subscriber 2 skipped 10 events across 2 lag episodes"));
    for run in ["sender", "recipient"] {
        assert!(matches!(
            registry.run(run).unwrap().entries(),
            [TranscriptEntry::AgentMessage { .. }]
        ));
    }
}
