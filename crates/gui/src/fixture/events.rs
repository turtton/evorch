//! demo transcript を構成する event 列。

use event_bus::{
    AgentMessage, AgentMessageEvent, AgentMessageKind, AgentRunPhase, DeliveryDisposition, Event,
    LifecycleEvent, MessageEvent, ProviderEvent, ToolEvent,
};

/// demo モードで transcript / phases / telemetry を満たす event 列。
pub fn demo_events() -> Vec<Event> {
    vec![
        run_started("run-1", "orchestrator", "orchestrator", None),
        run_state_changed("run-1", AgentRunPhase::Pending, AgentRunPhase::Running),
        run_started("run-2", "implementer", "worker", Some("run-1")),
        run_state_changed("run-2", AgentRunPhase::Pending, AgentRunPhase::Done),
        run_started("run-3", "reviewer", "reviewer", Some("run-1")),
        run_state_changed("run-3", AgentRunPhase::Pending, AgentRunPhase::Waiting),
        Event::new(MessageEvent::MessageDelta {
            delta: "Analysing t3code design language and mapping tokens to egui Visuals…".into(),
        }),
        Event::new(MessageEvent::ReasoningDelta {
            delta: "Sidebar darker than canvas; surfaces stay within one luminance step.".into(),
        }),
        tool_started("run-1", "read_file", "call-1"),
        delivered("run-1", "run-2", "Implement theme module"),
        request_started("run-1", "anthropic", "claude"),
        request_completed("run-1", 120, 34),
    ]
}

fn run_started(run_id: &str, agent_name: &str, role: &str, parent_run_id: Option<&str>) -> Event {
    Event::new(LifecycleEvent::AgentRunStarted {
        run_id: run_id.into(),
        parent_run_id: parent_run_id.map(str::to_owned),
        agent_name: agent_name.into(),
        role: role.into(),
    })
}

fn run_state_changed(run_id: &str, from: AgentRunPhase, to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: run_id.into(),
        from,
        to,
        reason: None,
    })
}

fn tool_started(run_id: &str, tool_name: &str, call_id: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: tool_name.into(),
        call_id: call_id.into(),
        run_id: Some(run_id.into()),
    })
}

fn delivered(sender: &str, recipient: &str, content: &str) -> Event {
    Event::new(AgentMessageEvent::Delivered {
        message: AgentMessage {
            message_id: format!("message-{sender}-{recipient}"),
            sender_run_id: sender.into(),
            recipient_run_id: recipient.into(),
            kind: AgentMessageKind::Send,
            content: content.into(),
            reply_to: None,
        },
        disposition: DeliveryDisposition::Aside,
    })
}

fn request_started(run_id: &str, provider: &str, model: &str) -> Event {
    Event::new(ProviderEvent::RequestStarted {
        request_id: format!("request-{run_id}"),
        provider: provider.into(),
        profile: None,
        protocol: "fixture".into(),
        model: model.into(),
        streaming: true,
        run_id: Some(run_id.into()),
    })
}

fn request_completed(run_id: &str, input_tokens: u64, output_tokens: u64) -> Event {
    Event::new(ProviderEvent::RequestCompleted {
        request_id: format!("request-{run_id}"),
        provider: "anthropic".into(),
        profile: None,
        protocol: "fixture".into(),
        model: "claude".into(),
        streaming: true,
        duration_ms: 10,
        input_tokens,
        output_tokens,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        finish_reason: "stop".into(),
        run_id: Some(run_id.into()),
    })
}
