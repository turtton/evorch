//! Coordination calls and calls still being composed render as tool cards.

#[path = "ledger_tool_calls/fixture.rs"]
mod fixture;

use egui_kittest::{Harness, kittest::Queryable};
use event_bus::{Event, MessageEvent, ToolEvent};
use fixture::execute;
use gui::model::transcript::{ToolStatus, TranscriptEntry};
use gui::model::transcript_registry::TranscriptRegistry;
use serde_json::json;

fn card(entry: &TranscriptEntry) -> Harness<'static> {
    let entry = entry.clone();
    let mut harness = Harness::new_ui(move |ui| {
        gui::theme::install(ui.ctx());
        gui::panes::transcript_tool::tool_card(ui, &entry, egui::Id::new("meta-card"));
    });
    harness.run_steps(2);
    harness
}

#[tokio::test]
async fn coordination_calls_reach_the_transcript_but_finish_does_not() {
    // Given: a real run that issues coordination calls (each ends in an error
    // result because its arguments are not usable) and then finishes.
    let calls = [
        ("delegate", json!({})),
        (
            "send_message",
            json!({"run_id": "run-404", "message": "hi"}),
        ),
        ("todo_write", json!({"items": "nope"})),
        ("get_goal", json!({})),
        ("finish", json!({"result": "all done"})),
    ];
    let (run, events) = execute(&calls, None).await;

    // When: the events are projected into the run's transcript.
    let mut registry = TranscriptRegistry::new();
    for event in &events {
        registry.apply(event);
    }

    // Then: every call except finish has one finished card, in call order.
    let tools: Vec<_> = registry
        .run(&run)
        .expect("run transcript")
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                tool_name, status, ..
            } => Some((tool_name.as_str(), status.clone())),
            _ => None,
        })
        .collect();
    let names: Vec<_> = tools.iter().map(|(name, _)| *name).collect();
    assert_eq!(
        names,
        ["delegate", "send_message", "todo_write", "get_goal"]
    );
    assert!(
        tools
            .iter()
            .all(|(_, status)| matches!(status, ToolStatus::Succeeded | ToolStatus::Failed)),
        "{tools:?}"
    );
}

#[tokio::test]
async fn a_delegate_card_names_the_agent_and_a_draft_shows_its_progress() {
    // Given: a delegate call that was confirmed after being composed.
    let mut registry = TranscriptRegistry::new();
    let draft = |text: &str, begins: bool| {
        Event::new(MessageEvent::ToolCallDelta {
            run_id: Some("run-1".into()),
            index: 0,
            call_id: begins.then(|| "call-1".to_owned()),
            tool_name: begins.then(|| "delegate".to_owned()),
            arguments_delta: text.into(),
        })
    };
    registry.apply(&draft("{\"name\":\"worker-w1\",\n", true));
    registry.apply(&draft("\"prompt\":\"fix the bug\"", false));

    // When: the draft is rendered mid-stream.
    let drafting = registry.run("run-1").unwrap().entries()[0].clone();
    assert!(matches!(
        drafting,
        TranscriptEntry::Tool {
            status: ToolStatus::Drafting { .. },
            ..
        }
    ));
    let harness = card(&drafting);

    // Then: the card shows the tool and the argument tail, and cannot be expanded.
    assert!(
        harness
            .query_by_label_contains("Delegate · composing…")
            .is_some()
    );
    assert!(
        harness
            .query_by_label_contains("\"prompt\":\"fix the bug\"")
            .is_some()
    );

    // And: once the runtime starts the call, the same card names the agent.
    registry.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "delegate".into(),
        call_id: "call-1".into(),
        input: Some(json!({"name": "worker-w1", "prompt": "fix the bug",
            "target": {"role": "worker"}})),
        run_id: Some("run-1".into()),
        parent_call_id: None,
    }));
    let entries = registry.run("run-1").unwrap().entries();
    assert_eq!(entries.len(), 1);
    let harness = card(&entries[0]);
    assert!(
        harness
            .query_by_label_contains("Delegate worker-w1 (worker)")
            .is_some()
    );
    assert!(harness.query_by_label_contains("composing…").is_none());
}
