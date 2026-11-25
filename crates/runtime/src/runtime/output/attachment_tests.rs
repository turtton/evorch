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
async fn retention_protection_distinguishes_live_phases_turn_boundaries_and_compaction() {
    let (runtime, _, child) = fixture();
    // No await until cleanup: the current-thread executor cannot race these
    // registered live snapshots with model execution or a phase publication.
    for (phase, idle_turn, compacting, protected) in [
        (AgentRunPhase::Pending, false, false, true),
        (AgentRunPhase::Running, false, false, true),
        (AgentRunPhase::Waiting, false, false, true),
        (AgentRunPhase::Waiting, true, false, false),
        (AgentRunPhase::Waiting, true, true, true),
        (AgentRunPhase::Done, false, false, false),
        (AgentRunPhase::Done, false, true, true),
    ] {
        set(&runtime, child, phase, idle_turn, None);
        lock_runs(&runtime.shared.runs)
            .get(&child)
            .unwrap()
            .compaction_busy
            .store(compacting, std::sync::atomic::Ordering::Release);
        assert_eq!(
            runtime.retention_run_protected(child),
            protected,
            "{phase:?}, idle_turn={idle_turn}, compacting={compacting}"
        );
    }
    lock_runs(&runtime.shared.runs)
        .get(&child)
        .unwrap()
        .compaction_busy
        .store(false, std::sync::atomic::Ordering::Release);
    cleanup(&runtime).await;
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

#[tokio::test]
async fn inputs_accepted_immediately_before_turn_publication_are_not_attachable() {
    use crate::escalation_review::support::{ScriptedModel, text_response};
    use crate::runtime::cancellation::HookPoint;
    use providers::FinishReason;
    for host in [false, true] {
        let bus = Arc::new(event_bus::EventBus::new(128));
        let model = Arc::new(ScriptedModel::new([]));
        let release_model = Arc::new(tokio::sync::Notify::new());
        model
            .gate_key("root", Arc::new(tokio::sync::Notify::new()))
            .await;
        model
            .add_keyed(
                "source",
                [Ok(text_response("first turn", FinishReason::Stop))],
            )
            .await;
        model.gate_key("source", release_model.clone()).await;
        let dir = tempfile::tempdir().unwrap();
        let storage_config = storage::StorageConfig {
            db_path: dir.path().join("publication.sqlite3"),
            ..Default::default()
        };
        let storage = storage::Storage::open(storage_config.clone()).unwrap();
        let runtime =
            AgentRuntime::new(bus.clone(), Arc::new(tools::ToolExecutor::new(bus)), model)
                .with_run_store(crate::RunStore::open(&storage_config, storage.handle()).unwrap());
        let root =
            runtime.delegate_background(Role::Orchestrator, "root".into(), RunConfig::default());
        let child = runtime
            .delegate_background_as_child(
                root,
                Role::Worker,
                "source",
                RunConfig {
                    interactive: true,
                    keep_alive: true,
                    ..RunConfig::default()
                },
            )
            .unwrap();
        let execution = runtime.execution_snapshot(child).unwrap();
        let sender = runtime.clone();
        runtime.on_execution(
            HookPoint::CompletedTurnBeforePublish,
            &execution,
            move || {
                if host {
                    sender
                        .send_user_input(child, "revise".into(), None)
                        .unwrap();
                } else {
                    sender
                        .send_agent_message(
                            root,
                            child,
                            event_bus::AgentMessageKind::Send,
                            "revise",
                            None,
                        )
                        .unwrap();
                }
            },
        );
        let (published, observed) = tokio::sync::oneshot::channel();
        let reader = runtime.clone();
        runtime.on_execution(HookPoint::CompletedTurnPublished, &execution, move || {
            let output = reader.run_output(root, child).unwrap();
            assert!(!output.completion_notice.as_ref().unwrap().observed());
            let value = serde_json::to_value(output).unwrap();
            let denied = err(&reader, root, child);
            published.send((value, denied)).unwrap();
        });
        release_model.notify_one();
        let (output, denied) = observed.await.unwrap();
        assert_eq!(output["phase"], "Waiting");
        assert_eq!(output["status"], "turn_completed");
        assert_eq!(output["output"], "first turn");
        assert!(output["turn_generation"].as_u64().is_some());
        assert!(output.get("output_ref").is_none());
        assert!(output.get("output_bytes").is_none());
        assert_eq!(denied["code"], "attachment_source_not_ready");
        cleanup(&runtime).await;
    }
}

fn prompt_event_bytes(parent: RunId, prompt: &str) -> usize {
    serde_json::to_vec(&event_bus::EventKind::Lifecycle(
        event_bus::LifecycleEvent::TaskPromptPublished {
            run_id: "run-7zzzzzzzzzzzzzzzzzzzzzzzzz".into(),
            parent_run_id: Some(parent.to_string()),
            agent_name: Role::Worker.name().into(),
            role: "worker".into(),
            prompt: prompt.into(),
            user_text: None,
            skills: Vec::new(),
        },
    ))
    .unwrap()
    .len()
}

#[tokio::test]
async fn team_task_final_prompt_limits_are_atomic_in_both_delegate_modes() {
    for background in [false, true] {
        for serialized in [false, true] {
            for delta in [-1_isize, 0, 1] {
                let (_interrupt, receiver) = tokio::sync::watch::channel(crate::RunInterrupt::None);
                let (runtime, mut state) =
                    crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
                let parent = state.caller_run_id();
                let source = runtime
                    .delegate_background_as_child(
                        parent,
                        Role::Planner,
                        "source",
                        RunConfig::default(),
                    )
                    .unwrap();
                set(
                    &runtime,
                    source,
                    AgentRunPhase::Done,
                    false,
                    Some("body".into()),
                );
                let team = crate::team_context::TeamContext::new(parent, 1);
                {
                    let mut runs = lock_runs(&runtime.shared.runs);
                    let entry = runs.get_mut(&parent).unwrap();
                    entry.config.team = Some(team.clone());
                    entry.config.delegation_value = Some("independent work".into());
                }
                let task = crate::team::TaskSpec {
                    id: if serialized {
                        "\"".repeat(40_000)
                    } else {
                        "team-task".into()
                    },
                    paths: vec!["src/日本語.rs".into()],
                };
                let config = RunConfig {
                    team_task: Some(task.clone()),
                    ..RunConfig::default()
                };
                let sources = vec![Source {
                    run: source,
                    output_ref: output_attachments::output_ref(
                        source,
                        OutputKind::Completed,
                        "body",
                    ),
                }];
                let expanded =
                    output_attachments::expand(&runtime, parent, String::new(), &sources).unwrap();
                let base = runtime.prepare_child_initial_prompt(parent, expanded, &config);
                let limit = if serialized {
                    storage::HardLimits::default().max_event_bytes as usize
                } else {
                    output_attachments::MAX_PROMPT_BYTES
                };
                let overhead = if serialized {
                    prompt_event_bytes(parent, &base)
                } else {
                    base.len()
                };
                let prompt = "x".repeat(limit.checked_add_signed(delta).unwrap() - overhead);
                let expanded =
                    output_attachments::expand(&runtime, parent, prompt.clone(), &sources).unwrap();
                // Both pre-team-task checks pass: only the final input crosses the boundary.
                output_attachments::validate_saved_prompt(parent, Role::Worker, None, &expanded)
                    .unwrap();
                let expected = runtime.prepare_child_initial_prompt(parent, expanded, &config);
                assert_eq!(
                    if serialized {
                        prompt_event_bytes(parent, &expected)
                    } else {
                        expected.len()
                    },
                    limit.checked_add_signed(delta).unwrap()
                );
                if serialized {
                    assert!(expected.len() < output_attachments::MAX_PROMPT_BYTES);
                }
                let mut events = runtime.shared.bus.subscribe();
                let before = runtime.list_agents().len();
                let result = crate::meta::spawn_delegate(
                    &mut state,
                    &runtime,
                    serde_json::json!({
                        "target":{"role":"worker"}, "prompt":prompt, "background":background, "task":task,
                        "output_attachments":[{"run_id":source.to_string(),"output_ref":sources[0].output_ref}]
                    }),
                );
                if delta > 0 {
                    let result =
                        result.expect_err("oversized final input must fail before registration");
                    assert!(result.result.is_error);
                    let error: serde_json::Value =
                        serde_json::from_str(&result.result.content).unwrap();
                    assert_eq!(error["code"], "attachment_too_large");
                    assert_eq!(
                        error["scope"],
                        if serialized {
                            "serialized_prompt_event_upper_bound"
                        } else {
                            "expanded_prompt"
                        }
                    );
                    assert_eq!(error["bytes"], limit + 1);
                    assert_eq!(runtime.list_agents().len(), before);
                    assert!(team.board.snapshot().unwrap().is_empty());
                    assert!(
                        team.reserve_worker().is_ok(),
                        "rejection consumes no worker slot"
                    );
                } else {
                    let child = match result {
                        Ok(child) => {
                            assert!(!background);
                            child
                        }
                        Err(result) => {
                            assert!(background);
                            assert!(!result.result.is_error, "{}", result.result.content);
                            result.result.content.parse().unwrap()
                        }
                    };
                    assert_eq!(runtime.list_agents().len(), before + 1);
                    assert_eq!(team.board.snapshot().unwrap()[0].spec, task);
                    loop {
                        let event = events.recv().await.unwrap();
                        if let event_bus::EventKind::Lifecycle(
                            event_bus::LifecycleEvent::TaskPromptPublished {
                                run_id, prompt, ..
                            },
                        ) = event.kind
                            && run_id == child.to_string()
                        {
                            assert_eq!(
                                prompt.as_bytes(),
                                expected.as_bytes(),
                                "the checked input is passed and saved verbatim, with no second task suffix"
                            );
                            assert!(
                                prompt_event_bytes(parent, &prompt)
                                    <= storage::HardLimits::default().max_event_bytes as usize
                            );
                            break;
                        }
                    }
                }
                cleanup(&runtime).await;
            }
        }
    }
}

#[tokio::test]
async fn publication_checks_adoption_even_after_queues_are_empty() {
    let (runtime, parent, child) = fixture();
    let (inbox, phase_tx) = {
        let runs = lock_runs(&runtime.shared.runs);
        let entry = runs.get(&child).unwrap();
        (entry.user_inbox.clone(), entry.phase_tx.clone())
    };
    let mut seq = 0;
    inbox
        .enqueue(None, |accepted| {
            seq = accepted;
            Ok::<_, ()>(())
        })
        .unwrap();
    let adoption = inbox.claim_for_adoption(seq).unwrap();
    assert_eq!(inbox.status().pending, 0);
    runtime.publish_live_phase(
        child,
        event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: child.to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
            waiting: None,
        },
        Some(Some("turn".into())),
        &phase_tx,
        || {},
    );
    let output = runtime.run_output(parent, child).unwrap();
    assert!(!output.completion_notice.as_ref().unwrap().observed());
    let value = serde_json::to_value(output).unwrap();
    assert_eq!(value["output"], "turn");
    assert!(value.get("output_ref").is_none());
    assert_eq!(
        err(&runtime, parent, child)["code"],
        "attachment_source_not_ready"
    );
    drop(adoption);
    runtime.publish_live_phase(
        child,
        event_bus::LifecycleEvent::AgentRunStateChanged {
            run_id: child.to_string(),
            from: AgentRunPhase::Running,
            to: AgentRunPhase::Waiting,
            reason: None,
            waiting: None,
        },
        Some(Some("new turn".into())),
        &phase_tx,
        || {},
    );
    assert!(
        serde_json::to_value(runtime.run_output(parent, child).unwrap())
            .unwrap()
            .get("output_ref")
            .is_some()
    );
    cleanup(&runtime).await;
}

#[tokio::test]
async fn team_delegates_without_attachments_keep_unchecked_prompt_bytes() {
    for background in [false, true] {
        for empty in [false, true] {
            let (_interrupt, receiver) = tokio::sync::watch::channel(crate::RunInterrupt::None);
            let (runtime, mut state) =
                crate::agent_loop::delegate_cleanup_tests::started_fixture(receiver).await;
            let parent = state.caller_run_id();
            let team = crate::team_context::TeamContext::new(parent, 1);
            {
                let mut runs = lock_runs(&runtime.shared.runs);
                let entry = runs.get_mut(&parent).unwrap();
                entry.config.team = Some(team.clone());
                entry.config.delegation_value = Some("independent work".into());
            }
            let task = crate::team::TaskSpec {
                id: "task".into(),
                paths: vec!["src/日本語.rs".into()],
            };
            let prompt = format!(
                "  caller\r\n日本語 {}",
                "x".repeat(output_attachments::MAX_PROMPT_BYTES)
            );
            let expected = runtime.prepare_child_initial_prompt(
                parent,
                prompt.clone(),
                &RunConfig {
                    team_task: Some(task.clone()),
                    ..RunConfig::default()
                },
            );
            assert!(expected.len() > output_attachments::MAX_PROMPT_BYTES);
            let mut input = serde_json::json!({"target":{"role":"worker"},"prompt":prompt,"background":background,"task":task});
            if empty {
                input["output_attachments"] = serde_json::json!([]);
            }
            let mut events = runtime.shared.bus.subscribe();
            let child = match crate::meta::spawn_delegate(&mut state, &runtime, input) {
                Ok(child) => {
                    assert!(!background);
                    child
                }
                Err(result) => {
                    assert!(background);
                    assert!(!result.result.is_error, "{}", result.result.content);
                    result.result.content.parse().unwrap()
                }
            };
            assert_eq!(team.board.snapshot().unwrap()[0].spec, task);
            loop {
                if let event_bus::EventKind::Lifecycle(
                    event_bus::LifecycleEvent::TaskPromptPublished { run_id, prompt, .. },
                ) = events.recv().await.unwrap().kind
                    && run_id == child.to_string()
                {
                    assert_eq!(prompt.as_bytes(), expected.as_bytes());
                    break;
                }
            }
            cleanup(&runtime).await;
        }
    }
}
