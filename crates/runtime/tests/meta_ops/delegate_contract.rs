use super::*;

#[tokio::test]
async fn delegate_is_single_meta_op_and_delegate_background_is_unknown_tool() {
    // Given: the obsolete name, assembled to keep retired tool literals out of callers.
    let old_name = ["delegate", "background"].join("_");
    let model = Arc::new(ScriptedModel::new([
        Ok(tool_response("old", &old_name, json!({"prompt": "unused"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]));
    let runtime = runtime_with(Arc::clone(&model));
    // When: an orchestrator attempts the obsolete tool.
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: the name is unregistered and cannot spawn a child.
    assert_eq!(
        runtime::META_OPS
            .iter()
            .filter(|&&name| name == "delegate")
            .count(),
        1
    );
    assert!(!runtime::META_OPS.contains(&old_name.as_str()));
    let observed = model.observed().await;
    let (content, failed) = tool_result(&observed[1], "old").expect("obsolete tool result");
    assert!(failed, "{content}");
    assert!(content.contains(&old_name), "{content}");
    assert_eq!(runtime.list_agents().len(), 1);
}

#[tokio::test]
async fn delegate_defaults_role_to_worker_when_omitted() {
    // Given: a foreground delegation with no role.
    let model = Arc::new(ScriptedModel::new([]));
    model
        .add_keyed(
            "ROOT",
            [
                Ok(tool_response(
                    "child",
                    "delegate",
                    json!({"prompt": "CHILD"}),
                )),
                Ok(text_response("done", FinishReason::Stop)),
            ],
        )
        .await;
    model
        .add_keyed(
            "CHILD",
            [Ok(text_response("child done", FinishReason::Stop))],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model));
    // When: the parent completes the awaited child.
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: the default role is Worker and the result is the foreground phase.
    let children = runtime.list_agents();
    assert_eq!(children.len(), 2);
    assert_eq!(children[1].role_name, "Worker");
    let observed = model.observed().await;
    assert!(
        observed
            .iter()
            .any(|turn| tool_result(turn, "child") == Some(("Done".into(), false)))
    );
}

#[tokio::test]
async fn delegate_rejects_interactive_without_background() {
    // Given: both absent and explicitly false background flags.
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ROOT", [
        Ok(tool_response("absent", "delegate", json!({"role": "worker", "prompt": "unused", "interactive": true}))),
        Ok(tool_response("false", "delegate", json!({"role": "worker", "prompt": "unused", "interactive": true, "background": false}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]).await;
    model
        .add_keyed(
            "unused",
            [
                Ok(text_response("child", FinishReason::Stop)),
                Ok(text_response("child", FinishReason::Stop)),
            ],
        )
        .await;
    let runtime = runtime_with(Arc::clone(&model));
    // When: both invalid requests cross the argument boundary.
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: no child is created and both errors identify the invalid combination.
    assert_eq!(runtime.list_agents().len(), 1);
    let observed = model.observed().await;
    for id in ["absent", "false"] {
        let (content, failed) = observed
            .iter()
            .find_map(|turn| tool_result(turn, id))
            .expect("invalid result");
        assert!(failed, "{content}");
        assert!(content.starts_with("invalid arguments:"), "{content}");
        assert!(
            content.contains("interactive") && content.contains("background"),
            "{content}"
        );
    }
}

#[tokio::test]
async fn delegate_background_true_returns_run_id_without_waiting() {
    // Given: an interactive child cannot finish until the parent cancels it.
    let model = Arc::new(ScriptedModel::new([]));
    model.add_keyed("ROOT", [
        Ok(tool_response("child", "delegate", json!({"role": "worker", "prompt": "CHILD", "background": true, "interactive": true}))),
        Ok(tool_response("stop-child", "cancel", json!({"run_id": "run-2"}))),
        Ok(text_response("done", FinishReason::Stop)),
    ]).await;
    model
        .add_keyed("CHILD", [Ok(text_response("waiting", FinishReason::Stop))])
        .await;
    let runtime = runtime_with(Arc::clone(&model));
    // When: the parent receives the delegation result before child completion.
    let parent =
        runtime.delegate_background(Role::Orchestrator, "ROOT".into(), RunConfig::default());
    assert_eq!(runtime.wait(parent).await, Ok(AgentRunPhase::Done));
    // Then: a run ID (not an awaited phase) permits cancellation from the next turn.
    let observed = model.observed().await;
    assert!(
        observed
            .iter()
            .any(|turn| tool_result(turn, "child") == Some(("run-2".into(), false)))
    );
    assert!(
        observed
            .iter()
            .any(|turn| tool_result(turn, "stop-child") == Some(("cancelled".into(), false)))
    );
    assert_eq!(
        runtime.wait(runtime::RunId::new(2)).await,
        Ok(AgentRunPhase::Error)
    );
}
