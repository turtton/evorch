use super::*;

fn observation(purpose: RequestPurpose) -> ObservationContext {
    ObservationContext {
        run_id: "run".into(),
        purpose,
        turn: None,
    }
}

fn previous(context: &ClaudeRequestContext) -> (Option<&str>, Option<&str>) {
    (
        context.previous_request_id.as_deref(),
        context.previous_message_id.as_deref(),
    )
}

#[test]
fn successful_attempts_chain_only_within_their_session_and_purpose() {
    let state = ClaudeTransportContext::default();
    let agent = observation(RequestPurpose::Agent);
    let first = state.begin("one", "device", Some(&agent)).unwrap();
    assert_eq!(previous(&first), (None, None));
    let first_id = first.request_id.clone();
    first.complete(Some("req_first"), Some("msg_first"));
    let second = state.begin("one", "device", Some(&agent)).unwrap();
    assert_ne!(first_id, second.request_id);
    assert_eq!(previous(&second), (Some("req_first"), Some("msg_first")));
    assert_eq!(
        previous(&state.begin("two", "device", Some(&agent)).unwrap()),
        (None, None)
    );
    assert_eq!(
        previous(&state.begin("one", "other-device", Some(&agent)).unwrap()),
        (None, None)
    );
    for purpose in [
        RequestPurpose::Compaction,
        RequestPurpose::Title,
        RequestPurpose::Routing,
    ] {
        let context = state
            .begin("one", "device", Some(&observation(purpose)))
            .unwrap();
        assert_eq!(previous(&context), (None, None));
        let mut headers = HeaderMap::new();
        context.apply_headers(&mut headers).unwrap();
        assert_eq!(
            headers["x-claude-code-request-class"],
            if purpose == RequestPurpose::Compaction {
                "compaction"
            } else {
                "auxiliary"
            }
        );
    }
}

#[test]
fn failure_cancellation_and_older_completion_cannot_replace_a_newer_success() {
    let state = ClaudeTransportContext::default();
    let old = state.begin("one", "device", None).unwrap();
    let newer = state.begin("one", "device", None).unwrap();
    newer.complete(Some("req_new"), Some("msg_new"));
    old.complete(Some("req_old"), Some("msg_old"));
    let canceled = state.begin("one", "device", None).unwrap();
    drop(canceled);
    let failed = state.begin("one", "device", None).unwrap();
    drop(failed);
    assert_eq!(
        previous(&state.begin("one", "device", None).unwrap()),
        (Some("req_new"), Some("msg_new"))
    );
}

#[test]
fn eviction_is_bounded_and_an_old_ticket_cannot_reenter_a_new_scope() {
    let state = ClaudeTransportContext::default();
    let old = state.begin("old", "device", None).unwrap();
    for index in 0..MAX_SCOPES {
        state.begin(&index.to_string(), "device", None).unwrap();
    }
    let fresh = state.begin("old", "device", None).unwrap();
    old.complete(Some("req_stale"), Some("msg_stale"));
    assert_eq!(
        previous(&state.begin("old", "device", None).unwrap()),
        (None, None)
    );
    fresh.complete(Some("req_fresh"), Some("msg_fresh"));
    assert_eq!(
        previous(&state.begin("old", "device", None).unwrap()),
        (Some("req_fresh"), Some("msg_fresh"))
    );
    assert_eq!(state.0.lock().unwrap().scopes.len(), MAX_SCOPES);
}

#[test]
fn prompt_provenance_is_validated_and_used_consistently() {
    let state = ClaudeTransportContext::default();
    let turn = RequestTurnContext {
        prompt_id: "33333333-3333-4333-8333-333333333333".into(),
        origin: Some("human".into()),
        prompt_index: Some(1),
        turn_index: Some(2),
        is_subagent: true,
        parent_subagent_id: Some("parent %\r\n😀".into()),
    };
    let mut input = observation(RequestPurpose::Agent);
    input.run_id = "child % 東京\r\nid".into();
    input.turn = Some(turn.clone());
    state
        .begin("one", "device", None)
        .unwrap()
        .complete(Some("req_first"), Some("msg_first"));
    let context = state.begin("one", "device", Some(&input)).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("oauth-2025-04-20"),
    );
    context.apply_headers(&mut headers).unwrap();
    context.apply_headers(&mut headers).unwrap();
    assert_eq!(
        headers["anthropic-beta"],
        "oauth-2025-04-20,cache-diagnosis-2026-04-07"
    );
    assert!(headers.get("x-claude-code-prev-tool-durations").is_none());
    assert_eq!(headers["x-claude-code-prompt-id"], turn.prompt_id);
    assert_eq!(headers["x-claude-code-request-class"], "subagent");
    assert_eq!(
        headers["x-claude-code-agent-id"],
        "child %25 %E6%9D%B1%E4%BA%AC%0D%0Aid"
    );
    assert_eq!(
        headers["x-claude-code-parent-agent-id"],
        "parent %25%0D%0A%F0%9F%98%80"
    );
    assert!(headers.get("x-claude-code-agent-type").is_none());
    for (purpose, expected) in [
        (RequestPurpose::Compaction, "compaction"),
        (RequestPurpose::Routing, "auxiliary"),
    ] {
        input.purpose = purpose;
        state
            .begin("one", "device", Some(&input))
            .unwrap()
            .apply_headers(&mut headers)
            .unwrap();
        assert_eq!(headers["x-claude-code-request-class"], expected);
    }
    let mut billing = "cch=00000;".to_string();
    context.append_billing(&mut billing);
    assert_eq!(
        billing,
        "cch=00000; cc_is_subagent=true; cc_prev_req=req_first; cc_prompt_id=33333333-3333-4333-8333-333333333333; cc_turn_origin=human; cc_prompt_index=1; cc_turn_index=2;"
    );
    assert_eq!(
        serde_json::to_value(context.diagnostics()).unwrap(),
        serde_json::json!({"previous_message_id":"msg_first"})
    );
    for (run, parent, child, expected_agent, expected_parent) in [
        ("agent", Some("parent"), false, None, None),
        ("", Some("parent"), true, None, Some("parent")),
        ("agent", None, true, Some("agent"), None),
        ("", Some(""), true, None, None),
    ] {
        let mut scoped = input.clone();
        scoped.run_id = run.into();
        scoped.turn.as_mut().unwrap().parent_subagent_id = parent.map(str::to_owned);
        scoped.turn.as_mut().unwrap().is_subagent = child;
        let scoped = state.begin("one", "device", Some(&scoped)).unwrap();
        let mut headers = HeaderMap::new();
        scoped.apply_headers(&mut headers).unwrap();
        assert_eq!(
            headers
                .get("x-claude-code-agent-id")
                .map(|value| value.to_str().unwrap()),
            expected_agent
        );
        assert_eq!(
            headers
                .get("x-claude-code-parent-agent-id")
                .map(|value| value.to_str().unwrap()),
            expected_parent
        );
    }
    for invalid in [
        RequestTurnContext {
            prompt_id: "unsafe; injected=1".into(),
            ..turn.clone()
        },
        RequestTurnContext {
            origin: Some("human; injected=1".into()),
            ..turn.clone()
        },
        RequestTurnContext {
            prompt_index: Some(MAX_INDEX + 1),
            ..turn.clone()
        },
        RequestTurnContext {
            turn_index: Some(0),
            ..turn
        },
    ] {
        input.turn = Some(invalid);
        assert!(state.begin("one", "device", Some(&input)).is_err());
    }
    state
        .begin("one", "device", None)
        .unwrap()
        .complete(Some("req_bad;extra=1"), Some("msg_\ninjection"));
    assert_eq!(
        previous(&state.begin("one", "device", None).unwrap()),
        (None, None)
    );
}
