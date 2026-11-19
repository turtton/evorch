use super::*;

async fn completed_turn(h: &mut Harness, run: RunId, events: &mut Vec<Event>) {
    loop {
        let event = h.receiver.recv().await.unwrap();
        let complete = matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::TurnCompleted {run_id,..}) if run_id==&run.to_string());
        assert!(
            !matches!(&event.kind, EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {run_id,to:AgentRunPhase::Error,..}) if run_id==&run.to_string()),
            "{event:?}"
        );
        events.push(event);
        if complete {
            break;
        }
    }
}

#[tokio::test]
async fn terminal_output_and_expanded_initial_input_survive_process_restart_without_reexpansion() {
    let body = format!(
        "  # Finished plan\r\n{}\n日本語🙂 [[PLAN]]  ",
        "x".repeat(40 * 1024)
    );
    let version = reference_with_status("run-2", "completed", &body);
    let mut h = harness(
        vec![
            call(
                "planner",
                "delegate",
                json!({"target":{"role":"worker"},"prompt":"TERMINAL_SOURCE"}),
            ),
            text_response(&body),
            call("source-ref", "run_output", json!({"run_id":"run-2"})),
            call(
                "review",
                "delegate",
                json!({"target":{"role":"reviewer"},"prompt":"RESTART_REVIEW","output_attachments":[{"run_id":"run-2","output_ref":version}]}),
            ),
            read_response(2),
            text_response("Review saved"),
            text_response("Waiting for input"),
        ],
        1_000_000,
    );
    let (config, db) = super::super::plan_cache::store(&mut h);
    let parent = h
        .runtime
        .delegate_chat(
            "attachments-restart",
            Role::Orchestrator,
            "RESTART_ROOT".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    let mut events = Vec::new();
    completed_turn(&mut h, parent, &mut events).await;
    let before = chat_requests(&h);
    let first = before
        .iter()
        .find(|r| {
            users(Wire::ChatCompletions, r)
                .first()
                .is_some_and(|s| s.starts_with("RESTART_REVIEW"))
        })
        .unwrap_or_else(|| panic!("missing reviewer: {:?}", trace(&h)));
    let original = users(Wire::ChatCompletions, first).remove(0);
    assert_quote(&original, "RESTART_REVIEW", &version, &body);
    let root = before
        .iter()
        .rev()
        .find(|r| {
            users(Wire::ChatCompletions, r)
                .first()
                .is_some_and(|s| s == "RESTART_ROOT")
        })
        .unwrap();
    assert_eq!(
        result(Wire::ChatCompletions, root, "source-ref")["output_ref"],
        version,
        "terminal completion metadata describes the full original body"
    );
    h.runtime
        .stop(parent, runtime::StopScope::SelfOnly)
        .unwrap();
    events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Stopped).await);
    h.runtime.wait(parent).await.unwrap();
    restart_runtime(&mut h, &config, &db);
    h.runtime
        .resume_chat(
            parent,
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, parent, AgentRunPhase::Waiting).await);
    // The source has no live interactive history to re-expand. Restoration must
    // use the receiver's already-saved first input regardless of source lookup.
    h.mock.enqueue([
        read_response(2),
        text_response("Restored review"),
        text_response("Observed restored review"),
    ]);
    h.runtime
        .send_agent_message(
            parent,
            RunId::new(3),
            event_bus::AgentMessageKind::Send,
            "Continue reviewing",
            None,
        )
        .unwrap();
    events.extend(through_phase(&mut h.receiver, RunId::new(3), AgentRunPhase::Done).await);
    h.runtime.wait(RunId::new(3)).await.unwrap();
    completed_turn(&mut h, parent, &mut events).await;
    h.runtime.cancel(parent).unwrap();
    h.runtime.wait(parent).await.unwrap();
    let all = chat_requests(&h);
    let reviewer = all
        .iter()
        .filter(|r| {
            users(Wire::ChatCompletions, r)
                .first()
                .is_some_and(|s| s.starts_with("RESTART_REVIEW"))
        })
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(reviewer.len(), 4);
    for request in &reviewer {
        assert_eq!(
            users(Wire::ChatCompletions, request)[0].as_bytes(),
            original.as_bytes()
        );
    }
    super::super::plan_cache::verify_root_prefix(&h, RunId::new(3), &events, &reviewer);
    assert_eq!(h.mock.remaining_scripts(), 0);
}
