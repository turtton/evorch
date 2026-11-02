use super::drafts::{PREVIEW_BYTES, RAW_WINDOW_BYTES};
use super::{ToolStatus, TranscriptEntry, TranscriptModel};
use event_bus::{AgentRunPhase, Event, LifecycleEvent, MessageEvent, ToolEvent};
use secret_guard::SecretRedactor;
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
        parent_call_id: None,
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

    // Then: the entry holds only a bounded tail and its newest text is intact.
    let (_, _, arguments) = &drafts(&model)[0];
    assert!(arguments.len() <= PREVIEW_BYTES);
    assert!(arguments.ends_with("END"));
    assert!(views(&model)[0].omitted, "the cut is reported");
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

#[test]
fn a_waiting_call_keeps_the_later_calls_of_its_batch() {
    // Given: one response with two calls; the first waits for a child.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-a"), Some("delegate"), "{}"));
    model.apply(&delta(1, Some("call-b"), Some("send_message"), "{}"));
    model.apply(&completed_message(""));
    model.apply(&started("call-a", "delegate"));

    // When: the run enters Waiting because call-a is awaiting its child.
    model.apply(&phase(AgentRunPhase::Waiting));

    // Then: call-b is still announced, and starts in place afterwards.
    assert_eq!(
        drafts(&model),
        [("send_message".into(), "call-b".into(), "{}".into())]
    );
    model.apply(&started("call-b", "send_message"));
    assert!(drafts(&model).is_empty());
    assert_eq!(model.entries().len(), 2);
}

#[test]
fn waiting_with_nothing_in_flight_is_idle_and_drops_sealed_drafts() {
    // Given: a finished call and a sealed draft that never started.
    let mut model = TranscriptModel::new();
    model.apply(&delta(0, Some("call-a"), Some("read"), "{}"));
    model.apply(&delta(1, Some("call-b"), Some("write"), "{}"));
    model.apply(&completed_message(""));
    model.apply(&started("call-a", "read"));
    model.apply(&Event::new(ToolEvent::ToolCompleted {
        tool_name: "read".into(),
        call_id: "call-a".into(),
        is_error: false,
        output: None,
        detail: None,
        run_id: Some(RUN.into()),
        parent_call_id: None,
    }));

    // When: the run waits for input.
    model.apply(&phase(AgentRunPhase::Waiting));

    // Then: the orphaned draft is dropped.
    assert!(drafts(&model).is_empty());
}

/// What each card shows: the decoded text, the argument tail and the omission flag.
struct Shown {
    text: String,
    arguments: String,
    omitted: bool,
}

fn views(model: &TranscriptModel) -> Vec<Shown> {
    model
        .entries()
        .iter()
        .filter_map(|entry| match entry {
            TranscriptEntry::Tool {
                status:
                    ToolStatus::Drafting {
                        text,
                        arguments,
                        omitted,
                        ..
                    },
                ..
            } => Some(Shown {
                text: text.clone(),
                arguments: arguments.clone(),
                omitted: *omitted,
            }),
            _ => None,
        })
        .collect()
}

/// The JSON a provider streams for `{"<field>": "<value>"}`, written the way a
/// model writes it: line breaks and quotes escaped.
fn json_string_arguments(prefix: &str, field: &str, value: &str) -> String {
    format!(
        "{{{prefix}\"{field}\":{}}}",
        serde_json::to_string(value).unwrap()
    )
}

/// Streams `arguments` in `step`-byte pieces (on character boundaries).
fn stream(model: &mut TranscriptModel, name: &str, arguments: &str, step: usize) {
    let mut rest = arguments;
    let mut first = true;
    while !rest.is_empty() {
        let mut cut = step.min(rest.len());
        while !rest.is_char_boundary(cut) {
            cut += 1;
        }
        let (piece, tail) = rest.split_at(cut);
        model.apply(&if first {
            delta(0, Some("call-1"), Some(name), piece)
        } else {
            delta(0, None, None, piece)
        });
        first = false;
        rest = tail;
    }
}

#[test]
fn a_multi_line_prompt_and_command_are_decoded_into_readable_text() {
    // Given: a delegate prompt and a shell command written with line breaks.
    let prompt = "Fix the parser.\n\n1. Read \"lexer.rs\"\n2. Add a test\n日本語も読める";
    let command = "cd crates/gui &&\n  cargo test \\\n  --locked";
    for (tool, field, text) in [
        ("delegate", "prompt", prompt),
        ("shell", "command", command),
    ] {
        let arguments = json_string_arguments("\"name\":\"w\",", field, text);
        // When: it streams in pieces of every small size, cutting inside escapes.
        for step in 1..=9 {
            let mut model = TranscriptModel::new();
            stream(&mut model, tool, &arguments, step);

            // Then: the card holds the decoded text, line breaks kept.
            let shown = &views(&model)[0];
            assert_eq!(shown.text, text, "{tool} step {step}");
            assert!(!shown.omitted);
        }
    }
}

#[test]
fn text_decoded_midway_never_shows_half_an_escape() {
    // Given/When: a prompt cut after every byte of the stream.
    let arguments = json_string_arguments("", "prompt", "a\nb \"q\" \u{1F600} c");
    for end in (0..=arguments.len()).filter(|end| arguments.is_char_boundary(*end)) {
        let mut model = TranscriptModel::new();
        model.apply(&delta(
            0,
            Some("call-1"),
            Some("delegate"),
            &arguments[..end],
        ));

        // Then: the decoded text is a prefix of the final one, with no stray backslash.
        let text = &views(&model)[0].text;
        assert!(
            "a\nb \"q\" \u{1F600} c".starts_with(text.as_str()),
            "{end}: {text:?}"
        );
    }
}

#[test]
fn a_prompt_beyond_the_old_two_kib_limit_is_readable_and_bounded() {
    // Given: a prompt of many lines, several times the former 2 KiB / 4 line view.
    let lines: Vec<String> = (0..600).map(|n| format!("line {n:03}: details")).collect();
    let prompt = lines.join("\n");
    assert!(prompt.len() > 4 * 2 * 1024);
    let arguments = json_string_arguments("", "prompt", &prompt);

    // When: it streams in.
    let mut model = TranscriptModel::new();
    stream(&mut model, "delegate", &arguments, 700);

    // Then: all of it is kept, because it fits the preview.
    let shown = &views(&model)[0];
    assert_eq!(shown.text, prompt);
    assert!(!shown.omitted);

    // And: one far beyond the window keeps its newest lines, says so, and stays bounded.
    let huge: Vec<String> = (0..20_000)
        .map(|n| format!("row {n:05} of the prompt"))
        .collect();
    let huge = huge.join("\n");
    let mut model = TranscriptModel::new();
    stream(
        &mut model,
        "delegate",
        &json_string_arguments("", "prompt", &huge),
        4000,
    );
    let shown = &views(&model)[0];
    assert!(shown.omitted);
    assert!(shown.text.len() <= PREVIEW_BYTES && shown.arguments.len() <= PREVIEW_BYTES);
    assert!(
        shown.text.ends_with("row 19999 of the prompt"),
        "{}",
        shown.text
    );
}

/// A token whose body has no run in common with ordinary words or the redaction markers.
const BODY: &str = "Zx9Qw3Lm7Tb2Vc5Nk8Rd4Hf6Jg1Ps0YaEu";

fn assert_hides_body(shown: &Shown, context: &str) {
    const LEAK: usize = 6;
    let chars: Vec<char> = BODY.chars().collect();
    for window in chars.windows(LEAK) {
        let fragment: String = window.iter().collect();
        for (what, text) in [("text", &shown.text), ("arguments", &shown.arguments)] {
            assert!(
                !text.contains(&fragment),
                "{context}: {fragment} leaked in {what}: {text}"
            );
        }
    }
}

#[test]
fn a_credential_is_hidden_at_every_fragment_boundary() {
    // Given: a command and a prompt that carry a token, plain and JSON-escaped.
    let token = format!("ghp_{BODY}");
    let escaped_opening = format!("\\u0067hp_{BODY}");
    for (tool, field) in [("shell", "command"), ("delegate", "prompt")] {
        for secret in [token.clone(), escaped_opening.clone()] {
            let value = format!("curl -H 'Authorization: {secret}'\nthen 日本語 more");
            let arguments =
                json_string_arguments("", field, &value).replace("\\\\u0067", "\\u0067");
            // When: the stream is cut after every character boundary.
            for end in (0..=arguments.len()).filter(|end| arguments.is_char_boundary(*end)) {
                let mut model = TranscriptModel::new();
                model.apply(&delta(0, Some("call-1"), Some(tool), &arguments[..end]));

                // Then: no part of the token is ever displayed.
                assert_hides_body(&views(&model)[0], &format!("{tool} at {end}"));
            }
        }
    }
}

#[test]
fn a_credential_cut_by_the_window_start_is_never_shown() {
    // Given: a token, then a private key block that redaction collapses to one
    // marker, so the cut token's remnant would sit inside the shown tail.
    let token = format!("ghp_{BODY}");
    let key = format!(
        "-----BEGIN PRIVATE KEY-----\n{}",
        "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC".repeat(RAW_WINDOW_BYTES / 40)
    );
    for cut_in in 0..=token.len() {
        let mut stream_text = format!("{token} ok {key}");
        // The window start lands `cut_in` bytes into the token.
        let excess = stream_text.len().saturating_sub(RAW_WINDOW_BYTES);
        let pad = cut_in.saturating_sub(excess);
        stream_text = format!("{token} ok {}{key}", "pad ".repeat(pad / 4));
        let total = stream_text.len();
        stream_text.push_str(&" w".repeat((RAW_WINDOW_BYTES + cut_in).saturating_sub(total) / 2));

        for step in [stream_text.len(), 5_000] {
            let mut model = TranscriptModel::new();
            let arguments = format!("{{\"command\":\"{}", stream_text.replace('\n', "\\n"));
            stream(&mut model, "shell", &arguments, step);

            // Then: the cut remnant is not displayed in either view.
            assert_hides_body(&views(&model)[0], &format!("cut {cut_in} step {step}"));
        }
    }
}

#[test]
fn the_unredacted_window_is_bounded_and_wider_than_any_credential() {
    // Given: the redactor the model uses, including a long configured value.
    let long = "v".repeat(9_000);
    let redactor = SecretRedactor::with_known_values([long]);

    // Then: the part of the window before the shown tail covers every credential
    // the redactor must mask, so a credential cut by the window start cannot
    // reach the tail.
    assert!(RAW_WINDOW_BYTES - PREVIEW_BYTES >= redactor.max_credential_len());
}

#[test]
fn drafts_of_different_calls_and_runs_keep_their_own_text() {
    // Given: two delegate drafts of one run and one of another run.
    let mut model = TranscriptModel::new();
    model.apply(&delta(
        0,
        Some("a"),
        Some("delegate"),
        "{\"prompt\":\"alpha\\none\"",
    ));
    model.apply(&delta(
        1,
        Some("b"),
        Some("shell"),
        "{\"command\":\"echo b\\n",
    ));
    model.apply(&Event::new(MessageEvent::ToolCallDelta {
        run_id: Some("other".into()),
        index: 0,
        call_id: Some("c".into()),
        tool_name: Some("delegate".into()),
        arguments_delta: "{\"prompt\":\"gamma".into(),
    }));
    model.apply(&delta(0, None, None, "}"));

    // Then: no text leaks across calls or runs.
    let texts: Vec<_> = views(&model).into_iter().map(|shown| shown.text).collect();
    assert_eq!(texts, ["alpha\none", "echo b\n", "gamma"]);
}

#[test]
fn a_draft_keeps_its_place_and_card_when_the_call_starts_and_a_retry_clears_the_text() {
    // Given: a long prompt being composed, then confirmed by the runtime.
    let mut model = TranscriptModel::new();
    model.apply(&text("Delegating."));
    model.apply(&delta(
        0,
        Some("call-1"),
        Some("delegate"),
        "{\"prompt\":\"first\\nsecond",
    ));
    let before = model.entries().len();
    model.apply(&Event::new(ToolEvent::ToolStarted {
        tool_name: "delegate".into(),
        call_id: "call-1".into(),
        input: Some(json!({"prompt": "first\nsecond"})),
        run_id: Some(RUN.into()),
        parent_call_id: None,
    }));

    // Then: the same entry became the running call, with the accepted input.
    assert_eq!(model.entries().len(), before);
    assert!(matches!(
        &model.entries()[1],
        TranscriptEntry::Tool { call_id, status: ToolStatus::Running, input: Some(input), .. }
            if call_id == "call-1" && input["prompt"] == "first\nsecond"
    ));

    // And: a retried attempt starts its draft from nothing.
    model.apply(&delta(
        0,
        Some("call-2"),
        Some("delegate"),
        "{\"prompt\":\"stale",
    ));
    model.apply(&retried());
    model.apply(&delta(
        0,
        Some("call-2"),
        Some("delegate"),
        "{\"prompt\":\"fresh",
    ));
    let texts: Vec<_> = views(&model).into_iter().map(|shown| shown.text).collect();
    assert_eq!(texts, ["fresh"]);
}
