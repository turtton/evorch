use super::{ToolStatus, TranscriptEntry, TranscriptModel};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use serde_json::json;

const RUN: &str = "run-1";

fn delta(index: usize, call: Option<&str>, name: Option<&str>, text: &str) -> Event {
    Event::new(MessageEvent::ToolCallDelta {
        run_id: Some(RUN.into()),
        index,
        call_id: call.map(Into::into),
        tool_name: name.map(Into::into),
        arguments_delta: text.into(),
    })
}

fn retried() -> Event {
    Event::new(MessageEvent::StreamRetried {
        run_id: Some(RUN.into()),
    })
}

fn phase(to: AgentRunPhase) -> Event {
    Event::new(LifecycleEvent::AgentRunStateChanged {
        run_id: RUN.into(),
        from: AgentRunPhase::Running,
        to,
        reason: None,
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
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"pa"));
    model.apply(&delta(0, None, None, "th\":\"a.rs\"}"));
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
    model.apply(&delta(0, Some("a"), Some("read"), "{\"x\""));
    model.apply(&delta(1, Some("b"), Some("grep"), "{\"y\""));
    model.apply(&delta(0, None, None, ":1}"));

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
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"stale"));

    // When: the provider retries and the new attempt replays the call from its start.
    model.apply(&retried());
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"fresh"));

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
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}"));
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
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}"));

    // When: more text arrives before the response completes.
    model.apply(&text("trailing words"));

    // Then: the call is still being composed.
    assert_eq!(drafts(&model).len(), 1);
}

#[test]
fn a_run_ending_discards_its_unconfirmed_drafts() {
    // Given: a draft and another run's draft.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}"));
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some("other".into()),
        index: 0,
        call_id: Some("call-2".into()),
        tool_name: Some("read".into()),
        arguments_delta: "{}".into(),
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
    model.apply(&delta(0, Some("call-1"), Some("finish"), "{\"result\":"));
    model.apply(&delta(0, None, None, "\"done\"}"));

    // Then: no card appears, since the answer is published as a message.
    assert!(model.entries().is_empty());
}

#[test]
fn a_call_without_the_provider_id_is_matched_by_tool_name() {
    // Given: a draft whose call ID never arrived.
    let mut model = TranscriptModel::new();
    model.apply(&delta(2, None, Some("write"), "{}"));

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
fn oversized_arguments_keep_a_bounded_redacted_tail() {
    // Given/When: far more text than a card can show streams in.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), ""));
    for _ in 0..40 {
        model.apply(&delta(0, None, None, &"x".repeat(8 * 1024)));
    }
    model.apply(&delta(0, None, None, "END"));

    // Then: the entry holds only a short tail and its newest text is intact.
    let (_, _, arguments) = &drafts(&model)[0];
    assert!(arguments.len() <= 2 * 1024);
    assert!(arguments.ends_with("END"));
}

#[test]
fn a_text_only_retry_discards_the_failed_attempts_draft() {
    // Given: a call was being composed when the stream failed.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"pa"));

    // When: the retry answers with text only, then the run goes idle.
    model.apply(&retried());
    model.apply(&text("Sorry, plain answer."));
    model.apply(&completed_message("Sorry, plain answer."));
    model.apply(&phase(AgentRunPhase::Waiting));

    // Then: no stale card is left behind the answer.
    assert!(drafts(&model).is_empty());
    assert_eq!(model.entries().len(), 1);
}

#[test]
fn an_idle_run_drops_drafts_its_response_never_started() {
    // Given: a finished response whose call never started.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}"));
    model.apply(&completed_message(""));
    assert_eq!(drafts(&model).len(), 1);

    // When: the run waits for input.
    model.apply(&phase(AgentRunPhase::Waiting));

    // Then: the card is gone.
    assert!(drafts(&model).is_empty());
}

#[test]
fn a_draft_far_behind_newer_entries_is_still_confirmed_and_discarded() {
    // Given: a draft followed by far more entries than any recent-window search covers.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("old"), Some("write"), "{}"));
    model.apply(&delta(1, Some("older"), Some("read"), "{}"));
    for index in 0..600 {
        model.push_notice(format!("notice {index}"));
    }

    // When: one draft is confirmed, and the stream later restarts.
    model.apply(&started("old", "write"));
    model.apply(&retried());

    // Then: the first became a running call in place and the other is gone.
    assert!(drafts(&model).is_empty());
    assert!(matches!(
        &model.entries()[0],
        TranscriptEntry::Tool { call_id, status: ToolStatus::Running, .. } if call_id == "old"
    ));
    assert!(model.entries().iter().all(|entry| !matches!(
        entry,
        TranscriptEntry::Tool { call_id, .. } if call_id == "older"
    )));
}

#[test]
fn evicted_drafts_leave_no_stale_tracking() {
    // Given: a bounded transcript that pushes the draft out.
    let mut model = TranscriptModel::with_capacity(2);
    model.apply(&delta(0, Some("call-1"), Some("write"), "{\"a\""));
    model.push_notice("one");
    model.push_notice("two");

    // When: a late nameless fragment of the evicted call arrives.
    model.apply(&delta(0, None, None, ":1}"));

    // Then: it is ignored rather than attached to some other entry.
    assert!(drafts(&model).is_empty());
    assert_eq!(model.entries().len(), 2);
}

#[test]
fn credentials_in_drafts_are_masked_even_when_split_across_fragments() {
    // Given: a token streamed in pieces, the first too short to be recognised.
    let mut model = TranscriptModel::new();
    let token = format!("ghp_{}", "AbCd1234".repeat(5));
    model.apply(&delta(
        0,
        Some("call-1"),
        Some("shell"),
        "{\"command\":\"curl -H ",
    ));
    model.apply(&delta(0, None, None, &token[..10]));
    let partial = &drafts(&model)[0].2;
    assert!(!partial.contains("ghp_AbCd"), "{partial}");

    // When: the rest arrives.
    model.apply(&delta(0, None, None, &token[10..]));
    model.apply(&delta(0, None, None, "\"}"));

    // Then: only a marker is displayed.
    let shown = &drafts(&model)[0].2;
    assert!(!shown.contains("AbCd1234"), "{shown}");
    assert!(shown.contains("[REDACTED:github-token]"), "{shown}");
}

#[test]
fn only_the_run_that_retries_loses_its_drafts() {
    // Given: drafts of two runs.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-1"), Some("write"), "{}"));
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some("other".into()),
        index: 0,
        call_id: Some("call-2".into()),
        tool_name: Some("read".into()),
        arguments_delta: "{}".into(),
    }));

    // When: one run retries.
    model.apply(&retried());

    // Then: the other run's draft stays.
    assert_eq!(
        drafts(&model),
        [("read".into(), "call-2".into(), "{}".into())]
    );
}
