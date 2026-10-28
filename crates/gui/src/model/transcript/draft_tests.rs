use super::{ToolStatus, TranscriptEntry, TranscriptModel};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use serde_json::json;

const RUN: &str = "run-1";

fn delta(index: usize, call: Option<&str>, name: Option<&str>, text: &str, begins: bool) -> Event {
    Event::new(MessageEvent::ToolCallDelta {
        run_id: Some(RUN.into()),
        index,
        call_id: call.map(Into::into),
        tool_name: name.map(Into::into),
        arguments_delta: text.into(),
        begins_attempt: begins,
    })
}

fn started(call: &str, name: &str) -> Event {
    Event::new(ToolEvent::ToolStarted {
        tool_name: name.into(),
        call_id: call.into(),
        input: Some(json!({"path": "a.rs"})),
        run_id: Some(RUN.into()),
    })
}

fn completed_message(text: &str) -> Event {
    Event::new(MessageEvent::MessageCompleted {
        run_id: RUN.into(),
        text: text.into(),
    })
}

fn text(text: &str) -> Event {
    Event::new(MessageEvent::MessageDelta {
        delta: text.into(),
        run_id: Some(RUN.into()),
    })
}

fn drafts(model: &TranscriptModel) -> Vec<(String, String, String)> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                tool_name,
                call_id,
                status: ToolStatus::Drafting { arguments, .. },
                ..
            } => Some((tool_name.clone(), call_id.clone(), arguments.clone())),
            _ => None,
        })
        .collect()
}

#[test]
fn fragments_accumulate_into_one_draft_that_tool_started_confirms_in_place() {
    // Given: a text entry followed by a call being composed.
    let mut model = TranscriptModel::new();
    model.apply(&text("Let me write that."));
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"pa", true));
    model.apply(&delta(0, None, None, "th\":\"a.rs\"}", false));
    assert_eq!(
        drafts(&model),
        [(
            "write".into(),
            "call-1".into(),
            "{\"path\":\"a.rs\"}".into()
        )]
    );

    // When: the accepted response starts the call.
    model.apply(&completed_message("Let me write that."));
    model.apply(&started("call-1", "write"));

    // Then: the same card becomes a running call with its parsed input.
    assert!(drafts(&model).is_empty());
    assert_eq!(model.entries().len(), 2);
    assert!(matches!(
        &model.entries()[1],
        TranscriptEntry::Tool {
            call_id,
            input: Some(input),
            status: ToolStatus::Running,
            ..
        } if call_id == "call-1" && input == &json!({"path": "a.rs"})
    ));
}

#[test]
fn parallel_calls_keep_separate_drafts_by_index() {
    // Given/When: two calls stream interleaved.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("a"), Some("read"), "{\"x\"", true));
    model.apply(&delta(1, Some("b"), Some("grep"), "{\"y\"", false));
    model.apply(&delta(0, None, None, ":1}", false));

    // Then: each keeps its own text.
    assert_eq!(
        drafts(&model),
        [
            ("read".into(), "a".into(), "{\"x\":1}".into()),
            ("grep".into(), "b".into(), "{\"y\"".into()),
        ]
    );
}

#[test]
fn a_retried_attempt_replaces_the_abandoned_draft() {
    // Given: a draft from an attempt that failed mid-call.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"stale", true));

    // When: the retry replays the call from its start.
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"fresh", true));

    // Then: only the new attempt's text remains.
    assert_eq!(
        drafts(&model),
        [("write".into(), "call-1".into(), "{\"fresh".into())]
    );
}

#[test]
fn sealed_drafts_survive_until_started_but_not_into_the_next_response() {
    // Given: a response that finished with a call that never starts.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}", true));
    model.apply(&completed_message(""));
    assert_eq!(drafts(&model).len(), 1, "waiting for its start event");

    // When: the next response streams text.
    model.apply(&text("Next answer"));

    // Then: the stale card is gone.
    assert!(drafts(&model).is_empty());
}

#[test]
fn text_inside_the_same_response_keeps_the_draft() {
    // Given: a draft whose response is still streaming.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}", true));

    // When: more text arrives before the response completes.
    model.apply(&text("trailing words"));

    // Then: the call is still being composed.
    assert_eq!(drafts(&model).len(), 1);
}

#[test]
fn a_run_ending_discards_its_unconfirmed_drafts() {
    // Given: a draft and another run's draft.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}", true));
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some("other".into()),
        index: 0,
        call_id: Some("call-2".into()),
        tool_name: Some("read".into()),
        arguments_delta: "{}".into(),
        begins_attempt: true,
    }));

    // When: the first run fails.
    model.apply(&Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: RUN.into(),
        from: AgentRunPhase::Running,
        to: AgentRunPhase::Error,
        reason: Some("boom".into()),
    }));

    // Then: only the other run keeps its draft.
    assert_eq!(
        drafts(&model),
        [("read".into(), "call-2".into(), "{}".into())]
    );
}

#[test]
fn finish_arguments_are_never_shown() {
    // Given/When: the model composes its final answer through finish.
    let mut model = TranscriptModel::new();
    model.apply(&delta(
        0,
        Some("call-1"),
        Some("finish"),
        "{\"result\":",
        true,
    ));
    model.apply(&delta(0, None, None, "\"done\"}", false));

    // Then: no card appears, since the answer is published as a message.
    assert!(model.entries().is_empty());
}

#[test]
fn a_call_without_the_provider_id_is_matched_by_tool_name() {
    // Given: a draft whose call ID never arrived.
    let mut model = TranscriptModel::new();
    model.apply(&delta(2, None, Some("write"), "{}", true));

    // When: the runtime reports the call under its own ID.
    model.apply(&started("runtime-id", "write"));

    // Then: the draft is confirmed rather than duplicated.
    assert_eq!(model.entries().len(), 1);
    assert!(matches!(
        &model.entries()[0],
        TranscriptEntry::Tool { call_id, status: ToolStatus::Running, .. } if call_id == "runtime-id"
    ));
}

#[test]
fn oversized_arguments_keep_their_tail() {
    // Given/When: far more text than the retained limit streams in.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "", true));
    for _ in 0..40 {
        model.apply(&delta(0, None, None, &"x".repeat(8 * 1024), false));
    }
    model.apply(&delta(0, None, None, "END", false));

    // Then: memory stays bounded and the newest text is intact.
    let (_, _, arguments) = &drafts(&model)[0];
    assert!(arguments.len() <= 128 * 1024 + 8 * 1024);
    assert!(arguments.ends_with("END"));
}
