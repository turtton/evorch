use super::*;
use crate::{AgentInvocationContext, AgentModel, Role, RunConfig};
use providers::{ChatResponse, Message, ToolSpec};
use std::sync::Arc;

struct Pending;
#[async_trait::async_trait]
impl AgentModel for Pending {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        _: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "pending".into()
    }
}
fn fixture() -> (AgentRuntime, RunId, RunId) {
    let bus = Arc::new(event_bus::EventBus::new(128));
    let runtime = AgentRuntime::new(
        bus.clone(),
        Arc::new(tools::ToolExecutor::new(bus)),
        Arc::new(Pending),
    );
    let root = runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
    let child = runtime
        .delegate_background_as_child(root, Role::Planner, "plan", RunConfig::default())
        .unwrap();
    (runtime, root, child)
}
// These tests exercise registered snapshots without scheduling model execution.
// The current-thread executor cannot publish another phase until an await.
fn set(
    runtime: &AgentRuntime,
    child: RunId,
    phase: AgentRunPhase,
    turn: bool,
    output: Option<String>,
) {
    let mut runs = lock_runs(&runtime.shared.runs);
    let e = runs.get_mut(&child).unwrap();
    e.phase_tx.send_replace(phase);
    if turn {
        e.idle_turn = Some(IdleTurn {
            output,
            attachable: true,
            notice: crate::mailbox::CompletionNotice::new(e.parent, child, 77),
        });
    } else {
        let (_, rx) = tokio::sync::watch::channel(output);
        e.result_rx = rx;
        e.idle_turn = None;
    }
}
fn sources(run: RunId) -> Vec<Source> {
    vec![Source {
        run,
        output_ref: String::new(),
    }]
}
fn err(runtime: &AgentRuntime, root: RunId, child: RunId) -> serde_json::Value {
    serde_json::from_str(
        &runtime
            .output_attachment_snapshots(root, &sources(child))
            .err()
            .expect("refused"),
    )
    .unwrap()
}
async fn cleanup(runtime: &AgentRuntime) {
    let runs = runtime.list_agents();
    for e in &runs {
        let _ = runtime.cancel(e.run_id);
    }
    for e in runs {
        runtime.wait(e.run_id).await.unwrap();
    }
}
#[tokio::test]
async fn relation_denials_are_indistinguishable_and_never_restore() {
    let (runtime, root, child) = fixture();
    let sibling = runtime
        .delegate_background_as_child(root, Role::Worker, "sibling", RunConfig::default())
        .unwrap();
    let grandchild = runtime
        .delegate_background_as_child(child, Role::Worker, "grandchild", RunConfig::default())
        .unwrap();
    let other = runtime.delegate_background(Role::Worker, "other".into(), RunConfig::default());
    let denied = err(&runtime, root, RunId::new(999));
    for (caller, target) in [
        (root, root),
        (root, grandchild),
        (root, other),
        (child, root),
        (child, sibling),
    ] {
        assert_eq!(err(&runtime, caller, target), denied);
    }
    let before = runtime.list_agents().len();
    assert_eq!(err(&runtime, root, RunId::new(998)), denied);
    assert_eq!(runtime.list_agents().len(), before);
    cleanup(&runtime).await;
}
#[tokio::test]
async fn completion_predicate_metadata_and_attachment_reads_have_no_observation_effect() {
    let (runtime, root, child) = fixture();
    for (phase, turn, body, expected) in [
        (
            AgentRunPhase::Pending,
            false,
            Some("x"),
            "attachment_source_not_ready",
        ),
        (
            AgentRunPhase::Running,
            false,
            Some("x"),
            "attachment_source_not_ready",
        ),
        (
            AgentRunPhase::Waiting,
            false,
            Some("x"),
            "attachment_source_not_ready",
        ),
        (
            AgentRunPhase::Error,
            false,
            Some("x"),
            "attachment_source_unsuccessful",
        ),
        (
            AgentRunPhase::Stopped,
            false,
            Some("x"),
            "attachment_source_unsuccessful",
        ),
        (
            AgentRunPhase::Done,
            false,
            None,
            "attachment_output_missing",
        ),
        (
            AgentRunPhase::Done,
            false,
            Some(""),
            "attachment_output_missing",
        ),
        (
            AgentRunPhase::Done,
            false,
            Some(" \r\n\t　"),
            "attachment_output_missing",
        ),
        (
            AgentRunPhase::Waiting,
            true,
            None,
            "attachment_output_missing",
        ),
        (
            AgentRunPhase::Waiting,
            true,
            Some("  "),
            "attachment_output_missing",
        ),
    ] {
        set(&runtime, child, phase, turn, body.map(str::to_owned));
        assert_eq!(err(&runtime, root, child)["code"], expected);
        let value = serde_json::to_value(runtime.run_output(root, child).unwrap()).unwrap();
        assert!(value.get("output_ref").is_none());
        assert!(value.get("output_bytes").is_none());
    }
    for (phase, turn, kind) in [
        (AgentRunPhase::Done, false, OutputKind::Completed),
        (AgentRunPhase::Waiting, true, OutputKind::TurnCompleted),
    ] {
        let body = "  日本語🙂\r\n";
        set(&runtime, child, phase, turn, Some(body.into()));
        let value = serde_json::to_value(runtime.run_output(root, child).unwrap()).unwrap();
        assert_eq!(
            value["output_ref"],
            output_attachments::output_ref(child, kind, body)
        );
        assert_eq!(value["output_bytes"], body.len());
        let notice = runtime
            .run_output(root, child)
            .unwrap()
            .completion_notice
            .unwrap();
        let version = *runtime.shared.turn_version.borrow();
        let count = runtime.list_agents().len();
        for _ in 0..2 {
            let snapshot = runtime
                .output_attachment_snapshots(root, &sources(child))
                .unwrap();
            assert_eq!(snapshot[0].output.as_bytes(), body.as_bytes());
        }
        assert!(!notice.observed());
        assert_eq!(*runtime.shared.turn_version.borrow(), version);
        assert_eq!(runtime.list_agents().len(), count);
        set(
            &runtime,
            root,
            AgentRunPhase::Done,
            false,
            Some("parent output".into()),
        );
        let parent = serde_json::to_value(runtime.run_output(child, root).unwrap()).unwrap();
        assert!(parent.get("output_ref").is_none());
    }
    cleanup(&runtime).await;
}
#[tokio::test]
async fn raw_limits_bound_copies_at_each_boundary_including_multibyte() {
    let (runtime, root, child) = fixture();
    for size in [
        output_attachments::MAX_OUTPUT_BYTES - 1,
        output_attachments::MAX_OUTPUT_BYTES,
        output_attachments::MAX_OUTPUT_BYTES + 1,
    ] {
        let text = format!("{}{}", "日".repeat(size / 3), "x".repeat(size % 3));
        assert_eq!(text.len(), size);
        set(&runtime, child, AgentRunPhase::Done, false, Some(text));
        if size <= output_attachments::MAX_OUTPUT_BYTES {
            assert_eq!(
                runtime
                    .output_attachment_snapshots(root, &sources(child))
                    .unwrap()[0]
                    .output
                    .len(),
                size
            );
        } else {
            assert_eq!(err(&runtime, root, child)["code"], "attachment_too_large");
        }
    }
    let second = runtime
        .delegate_background_as_child(root, Role::Worker, "2", RunConfig::default())
        .unwrap();
    let third = runtime
        .delegate_background_as_child(root, Role::Worker, "3", RunConfig::default())
        .unwrap();
    set(
        &runtime,
        child,
        AgentRunPhase::Done,
        false,
        Some("x".repeat(output_attachments::MAX_OUTPUT_BYTES)),
    );
    set(
        &runtime,
        second,
        AgentRunPhase::Done,
        false,
        Some("x".repeat(output_attachments::MAX_OUTPUT_BYTES - 1)),
    );
    set(
        &runtime,
        third,
        AgentRunPhase::Done,
        false,
        Some("x".into()),
    );
    let mut all = sources(child);
    all.extend(sources(second));
    assert!(runtime.output_attachment_snapshots(root, &all).is_ok());
    all.extend(sources(third));
    assert!(runtime.output_attachment_snapshots(root, &all).is_ok());
    set(
        &runtime,
        third,
        AgentRunPhase::Done,
        false,
        Some("xx".into()),
    );
    let error: serde_json::Value = serde_json::from_str(
        &runtime
            .output_attachment_snapshots(root, &all)
            .err()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(error["code"], "attachment_too_large");
    assert_eq!(error["bytes"], output_attachments::MAX_TOTAL_BYTES + 1);
    cleanup(&runtime).await;
}

#[tokio::test]
async fn new_input_invalidates_attachment_without_consuming_the_old_output() {
    let (runtime, root, child) = fixture();
    set(
        &runtime,
        child,
        AgentRunPhase::Waiting,
        true,
        Some("version one".into()),
    );
    let before = serde_json::to_value(runtime.run_output(root, child).unwrap()).unwrap();
    let sibling = runtime
        .delegate_background_as_child(root, Role::Worker, "sibling", RunConfig::default())
        .unwrap();
    assert!(
        runtime
            .send_agent_message(
                sibling,
                child,
                event_bus::AgentMessageKind::Send,
                "denied",
                None
            )
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(runtime.run_output(root, child).unwrap()).unwrap()["output_ref"],
        before["output_ref"]
    );
    runtime
        .send_agent_message(
            root,
            child,
            event_bus::AgentMessageKind::Send,
            "revise",
            None,
        )
        .unwrap();
    assert_eq!(
        err(&runtime, root, child)["code"],
        "attachment_source_not_ready"
    );
    let after = runtime.run_output(root, child).unwrap();
    assert!(!after.completion_notice.as_ref().unwrap().observed());
    let after = serde_json::to_value(after).unwrap();
    assert_eq!(
        after["output"], before["output"],
        "existing output and ACK semantics"
    );
    assert!(after.get("output_ref").is_none());
    cleanup(&runtime).await;
}

#[tokio::test]
async fn no_attachments_preserves_prompt_bytes_even_without_a_registered_caller() {
    let (runtime, _, _) = fixture();
    let prompt = format!(
        "  [[PLAN]]\r\n日本語🙂 {}",
        "x".repeat(output_attachments::MAX_PROMPT_BYTES)
    );
    assert_eq!(
        output_attachments::expand(&runtime, RunId::new(999), prompt.clone(), &[])
            .unwrap()
            .as_bytes(),
        prompt.as_bytes()
    );
    cleanup(&runtime).await;
}

#[tokio::test]
async fn invalid_attachment_delegates_do_not_create_partial_children_in_either_mode() {
    let (_interrupt, receiver) = tokio::sync::watch::channel(crate::RunInterrupt::None);
    let (runtime, mut state) =
        crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
    let root = state.caller_run_id();
    let child = runtime
        .delegate_background_as_child(root, Role::Planner, "source", RunConfig::default())
        .unwrap();
    set(
        &runtime,
        child,
        AgentRunPhase::Done,
        false,
        Some("source output".into()),
    );
    let version = output_attachments::output_ref(child, OutputKind::Completed, "source output");
    let valid = serde_json::json!({"run_id":child.to_string(),"output_ref":version});
    let changed = serde_json::json!({"run_id":child.to_string(),"output_ref":format!("out-v1-sha256:{}", "0".repeat(64))});
    let denied = serde_json::json!({"run_id":root.to_string(),"output_ref":version});
    for background in [false, true] {
        for (attachments, code) in [
            (serde_json::Value::Null, "invalid_output_attachments"),
            (serde_json::json!([{}]), "invalid_output_attachments"),
            (
                serde_json::json!([valid.clone(), valid.clone()]),
                "invalid_output_attachments",
            ),
            (
                serde_json::json!([valid.clone(), denied.clone()]),
                "attachment_source_denied",
            ),
            (
                serde_json::json!([changed.clone()]),
                "attachment_output_changed",
            ),
        ] {
            let count = runtime.list_agents().len();
            let result = crate::meta::spawn_delegate(&mut state, &runtime, serde_json::json!({
                "target":{"role":"reviewer"},"prompt":"review","background":background,"output_attachments":attachments
            })).expect_err("must fail before spawning");
            assert!(result.result.is_error);
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result.result.content).unwrap()["code"],
                code
            );
            assert_eq!(runtime.list_agents().len(), count);
        }
    }
    cleanup(&runtime).await;
}

#[tokio::test]
async fn pending_admission_is_not_ready_only_for_its_live_direct_parent() {
    let (runtime, parent, child) = fixture();
    let reserved = "run-90001".parse::<RunId>().unwrap();
    let execution = runtime.reserve_admission(reserved, Some(parent), None);
    let source =
        crate::output_attachments::validate(vec![crate::output_attachments::OutputAttachment {
            run_id: reserved.to_string(),
            output_ref: output_attachments::output_ref(
                reserved,
                OutputKind::Completed,
                "not published",
            ),
        }])
        .unwrap();
    for caller in [parent, child] {
        let error = runtime
            .output_attachment_snapshots(caller, &source)
            .err()
            .expect("unfinished admission has no attachable output");
        assert!(
            error.contains(if caller == parent {
                "attachment_source_not_ready"
            } else {
                "attachment_source_denied"
            }),
            "{error}"
        );
    }
    assert!(
        runtime
            .admission_snapshot(reserved)
            .unwrap()
            .result
            .is_none()
    );
    assert!(!lock_runs(&runtime.shared.runs).contains_key(&reserved));
    runtime.fail_reserved_admission(reserved, &execution, "never registered".into());
    assert!(
        runtime
            .output_attachment_snapshots(parent, &source)
            .err()
            .expect("a failed unregistered admission remains unavailable")
            .contains("attachment_source_denied")
    );
    cleanup(&runtime).await;
}

#[tokio::test]
async fn successful_delegation_paths_publish_frozen_expansion_and_leave_empty_inputs_exact() {
    for background in [false, true] {
        let (_interrupt, receiver) = tokio::sync::watch::channel(crate::RunInterrupt::None);
        let (runtime, mut state) =
            crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
        let root = state.caller_run_id();
        let source = runtime
            .delegate_background_as_child(root, Role::Planner, "source", RunConfig::default())
            .unwrap();
        let body = format!("  引用🙂\r\n{} [[PLAN]]  ", "x".repeat(40 * 1024));
        set(
            &runtime,
            source,
            AgentRunPhase::Done,
            false,
            Some(body.clone()),
        );
        let version = output_attachments::output_ref(source, OutputKind::Completed, &body);
        let mut events = runtime.shared.bus.subscribe();
        for attachments in [
            Some(serde_json::json!([{"run_id":source.to_string(),"output_ref":version}])),
            Some(serde_json::json!([])),
            None,
        ] {
            // Refresh only the synthetic source before the next spawn; a child's
            // published input must not depend on its later live state.
            set(
                &runtime,
                source,
                AgentRunPhase::Done,
                false,
                Some(body.clone()),
            );
            let prompt = "  caller\r\n日本語 [[PLAN]]  ";
            let mut input = serde_json::json!({"target":{"role":"worker"},"prompt":prompt,"background":background});
            if let Some(value) = &attachments {
                input["output_attachments"] = value.clone();
            }
            let child = match crate::meta::spawn_delegate(&mut state, &runtime, input) {
                Ok(child) => {
                    assert!(!background);
                    child
                }
                Err(result) => {
                    assert!(background);
                    assert!(!result.result.is_error);
                    result.result.content.parse().unwrap()
                }
            };
            loop {
                let event = events.recv().await.unwrap();
                if let event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::TaskPromptPublished {
                        run_id,
                        prompt: published,
                        user_text,
                        skills,
                        ..
                    },
                ) = event.kind
                    && run_id == child.to_string()
                {
                    assert!(user_text.is_none());
                    assert!(
                        skills.is_empty(),
                        "quoted text creates no trusted user/skill input"
                    );
                    if attachments
                        .as_ref()
                        .and_then(|v| v.as_array())
                        .is_some_and(|v| !v.is_empty())
                    {
                        assert!(published.starts_with(prompt));
                        assert!(published.contains(&version));
                        assert!(published.ends_with(&body));
                    } else {
                        assert_eq!(published.as_bytes(), prompt.as_bytes());
                    }
                    break;
                }
            }
        }
        cleanup(&runtime).await;
    }
}
