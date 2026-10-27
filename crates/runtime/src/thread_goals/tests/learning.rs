use super::*;

fn enable(runtime: AgentRuntime, model: &Model) -> AgentRuntime {
    runtime.with_learning(crate::memory_queue::LearningSettings {
        writer: model._storage.handle(),
        storage: storage::StorageConfig {
            db_path: model._dir.path().join("review.db"),
            ..Default::default()
        },
        project: "test".into(),
        quick: crate::ModelPreference {
            profile: "quick".into(),
            model: None,
            reasoning_effort: None,
        },
    })
}

#[tokio::test]
async fn goal_learning_without_source_inspection_fails_and_emits_diagnostic() {
    let (runtime, model, mut events) = harness(
        vec![Step::Create, Step::Stop, Step::Check(true), Step::Stop],
        vec![],
    );
    model
        .inspect_lessons
        .store(false, std::sync::atomic::Ordering::Relaxed);
    let runtime = enable(runtime, &model);
    let mut diagnostics = runtime.shared.bus.subscribe();
    let root = start(&runtime, Role::Worker, true);
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
    let error = runtime
        .wait_learning(root)
        .await
        .unwrap()
        .expect_err("a prepared snapshot is not a successful source inspection");
    assert!(error.contains("extraction did not inspect a persisted learning source"));
    // wait_learning resolves after the pipeline publishes its failure diagnostic.
    let diagnostic = diagnostics
        .drain_pending_snapshot()
        .into_iter()
        .find_map(|event| {
            if let event_bus::EventKind::Diagnostic(diagnostic) = event.kind
                && diagnostic.code == event_bus::event::diagnostic_codes::LEARNING_PIPELINE_FAILED
            {
                Some(diagnostic)
            } else {
                None
            }
        })
        .expect("learning failure diagnostic");
    assert_eq!(diagnostic.run_id, Some(root.to_string()));
    assert!(diagnostic.detail.contains(&error));
    let extractor = runtime
        .list_agents()
        .into_iter()
        .find(|run| run.category.as_deref() == Some("lesson"))
        .unwrap()
        .run_id;
    assert_eq!(
        runtime.inspect_agent(extractor).unwrap().phase,
        AgentRunPhase::Done
    );
    assert_eq!(
        runtime.inspect_agent(root).unwrap().phase,
        AgentRunPhase::Waiting
    );
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn keep_alive_complete_starts_learning_once_without_changing_conversation_prefix() {
    let (runtime, model, mut events) = harness(
        vec![
            Step::Create,
            Step::Stop,
            Step::Check(true),
            Step::Stop,
            Step::Stop,
            Step::Stop,
        ],
        vec![],
    );
    let runtime = enable(runtime, &model);
    let root = start(&runtime, Role::Worker, true);
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
    runtime.wait_learning(root).await.unwrap().unwrap();
    assert_eq!(
        runtime
            .list_agents()
            .iter()
            .filter(|run| run.category.as_deref() == Some("lesson"))
            .count(),
        1,
        "Complete on a live conversation must trigger an internal extraction"
    );
    let completed_epoch = runtime.thread_goal("thread").unwrap().epoch;
    for restore in [false, true] {
        if restore {
            runtime
                .restore_thread_goal(runtime.thread_goal("thread").unwrap())
                .unwrap();
        }
        runtime.send_message(root, "Thank you".into()).unwrap();
        phase(&mut events, root, AgentRunPhase::Waiting).await;
        runtime.wait_learning(root).await.unwrap().unwrap();
        assert_eq!(
            runtime
                .list_agents()
                .iter()
                .filter(|run| run.category.as_deref() == Some("lesson"))
                .count(),
            1
        );
        if !restore {
            assert_eq!(
                runtime.thread_goal("thread").unwrap().epoch,
                completed_epoch
            );
        }
    }
    let requests = model.requests.lock().unwrap().clone();
    let root_requests: Vec<_> = requests
        .iter()
        .filter(|request| request.0 == root.to_string())
        .collect();
    for pair in root_requests.windows(2) {
        assert_eq!(pair[0].2, pair[1].2);
        assert_eq!(pair[1].1[..pair[0].1.len()], pair[0].1);
    }
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Stopped);
}

#[tokio::test]
async fn escalation_complete_captures_both_roots_and_goal_evidence() {
    let (runtime, model, mut events) = harness(
        vec![
            Step::Create,
            Step::Escalate,
            Step::Stop,
            Step::Check(true),
            Step::Stop,
        ],
        vec![],
    );
    let runtime = enable(runtime, &model);
    let source = start(&runtime, Role::Worker, true);
    let target = loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
            new_run_id,
            ..
        }) = events.recv().await.unwrap().kind
        {
            break crate::meta::parse_run_id(&new_run_id).unwrap();
        }
    };
    phase(&mut events, target, AgentRunPhase::Waiting).await;
    runtime.wait_learning(target).await.unwrap().unwrap();
    let goal = runtime.goal_for_root(target).unwrap();
    assert_eq!(goal.related_root_run_ids, vec![source.to_string()]);
    let extractor = runtime
        .list_agents()
        .into_iter()
        .find(|run| run.category.as_deref() == Some("lesson"))
        .unwrap()
        .run_id;
    let requests = model.requests.lock().unwrap().clone();
    let input = requests
        .iter()
        .rfind(|request| request.0 == extractor.to_string())
        .unwrap()
        .1
        .clone();
    let list = input
        .iter()
        .flat_map(|message| &message.content)
        .find_map(|block| {
            if let ContentBlock::ToolResult {
                tool_call_id,
                content,
                is_error,
            } = block
                && tool_call_id == "learning-list"
            {
                assert!(!is_error, "{content:?}");
                Some(
                    serde_json::from_str::<serde_json::Value>(&content[0].text_or_image_note())
                        .unwrap(),
                )
            } else {
                None
            }
        })
        .unwrap();
    let ids: Vec<_> = list["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|run| run["run_id"].as_str().unwrap())
        .collect();
    assert!(ids.contains(&source.to_string().as_str()));
    assert!(ids.contains(&target.to_string().as_str()));
    let prompt = serde_json::to_string(&input).unwrap();
    for value in [&goal.objective, &goal.criteria[0], &goal.checks[0].evidence] {
        assert!(
            prompt.contains(value),
            "goal evidence must reach extraction"
        );
    }
    assert_eq!(runtime.wait(source).await.unwrap(), AgentRunPhase::Done);
    runtime.stop(target, StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
}

#[tokio::test]
async fn no_goal_unfinished_and_blocked_do_not_start_learning() {
    for state in ["no-goal", "unfinished", "blocked"] {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let steps = if state == "no-goal" {
            vec![Step::Stop]
        } else {
            vec![Step::Create, Step::Hold(entered.clone(), release.clone())]
        };
        let (runtime, model, mut events) = harness(steps, vec![]);
        let runtime = enable(runtime, &model);
        let root = start(&runtime, Role::Worker, true);
        if state != "no-goal" {
            entered.notified().await;
            let goal = runtime.thread_goal("thread").unwrap();
            if state == "blocked" {
                runtime
                    .goal_lock()
                    .goals
                    .get_mut("thread")
                    .unwrap()
                    .snapshot
                    .phase = ThreadGoalPhase::Blocked;
            } else {
                runtime
                    .set_goal_checks_paused("thread", &goal.goal_id, true)
                    .unwrap();
            }
            release.notify_one();
        }
        phase(&mut events, root, AgentRunPhase::Waiting).await;
        assert!(
            runtime.shared.learning_runs.lock().unwrap().is_empty(),
            "{state}"
        );
        assert!(
            !runtime
                .list_agents()
                .iter()
                .any(|run| run.category.as_deref() == Some("lesson"))
        );
        runtime.stop(root, StopScope::SelfOnly).unwrap();
        runtime.wait(root).await.unwrap();
    }
}

#[tokio::test]
async fn disabled_collection_and_zero_daily_limit_do_not_start_learning() {
    for (collect_lessons, daily_limit) in [(false, 20), (true, 0)] {
        let (runtime, model, mut events) = harness(
            vec![Step::Create, Step::Stop, Step::Check(true), Step::Stop],
            vec![],
        );
        let runtime = enable(runtime, &model).with_self_improvement(
            crate::self_improvement::ImprovementSettings {
                writer: model._storage.handle(),
                project: "test".into(),
                policy: crate::self_improvement::ImprovementPolicy {
                    collect_lessons,
                    daily_limit,
                    ..Default::default()
                },
            },
        );
        let root = start(&runtime, Role::Worker, true);
        phase(&mut events, root, AgentRunPhase::Waiting).await;
        assert_eq!(
            runtime.thread_goal("thread").unwrap().phase,
            ThreadGoalPhase::Complete
        );
        assert!(runtime.shared.learning_runs.lock().unwrap().is_empty());
        runtime.stop(root, StopScope::SelfOnly).unwrap();
        runtime.wait(root).await.unwrap();
    }
}

#[tokio::test]
async fn finite_root_uses_goal_path_only_and_learning_never_recurses() {
    let (runtime, model, _) = harness(
        vec![Step::Create, Step::Stop, Step::Check(true), Step::Stop],
        vec![],
    );
    let runtime = enable(runtime, &model);
    let root = start(&runtime, Role::Worker, false);
    runtime.wait(root).await.unwrap();
    runtime.wait_learning(root).await.unwrap().unwrap();
    let agents = runtime.list_agents();
    assert_eq!(agents.len(), 2);
    let learning = agents.iter().find(|run| run.run_id != root).unwrap().run_id;
    assert!(
        !runtime
            .shared
            .learning_runs
            .lock()
            .unwrap()
            .contains_key(&learning)
    );
}

#[tokio::test]
async fn completed_child_is_not_a_learning_trigger() {
    // Child completion can wake its interactive parent with a report.
    let (runtime, model, mut events) = harness(vec![Step::Stop, Step::Stop], vec![]);
    model.child.lock().unwrap().push_back(Step::Stop);
    let runtime = enable(runtime, &model);
    let root = start(&runtime, Role::Worker, true);
    phase(&mut events, root, AgentRunPhase::Waiting).await;
    let child = runtime
        .delegate_background_as_child(root, Role::Explorer, "Child work", RunConfig::default())
        .unwrap();
    assert_eq!(runtime.wait(child).await.unwrap(), AgentRunPhase::Done);
    assert!(
        !runtime
            .shared
            .learning_runs
            .lock()
            .unwrap()
            .contains_key(&child)
    );
    assert_eq!(runtime.list_agents().len(), 2);
    runtime.stop(root, StopScope::SelfOnly).unwrap();
    runtime.wait(root).await.unwrap();
}

#[tokio::test]
async fn independently_reviewed_completion_also_starts_learning() {
    let (runtime, model, _) = harness(
        vec![Step::Stop, Step::Check(true), Step::Stop],
        vec![Step::Review(true), Step::Stop],
    );
    let runtime = enable(runtime, &model);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    let goal = runtime
        .create_thread_goal(
            "thread",
            root,
            "Research options".into(),
            vec!["Verified report".into()],
        )
        .unwrap();
    runtime
        .set_goal_review("thread", &goal.goal_id, true)
        .unwrap();
    runtime.spawn_reserved(
        root,
        None,
        Role::Worker,
        "Research options",
        RunConfig::default(),
    );
    runtime.wait(root).await.unwrap();
    runtime.wait_learning(root).await.unwrap().unwrap();
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
    assert_eq!(
        runtime
            .list_agents()
            .iter()
            .filter(|run| run.category.as_deref() == Some("lesson"))
            .count(),
        1
    );
}

#[tokio::test]
async fn escalation_without_a_goal_is_not_a_trigger() {
    let (runtime, model, mut events) = harness(vec![Step::Escalate, Step::Stop], vec![]);
    let runtime = enable(runtime, &model);
    let source = start(&runtime, Role::Worker, true);
    let target = loop {
        if let event_bus::EventKind::Lifecycle(event_bus::LifecycleEvent::EscalationRequested {
            new_run_id,
            ..
        }) = events.recv().await.unwrap().kind
        {
            break crate::meta::parse_run_id(&new_run_id).unwrap();
        }
    };
    phase(&mut events, target, AgentRunPhase::Waiting).await;
    runtime.wait(source).await.unwrap();
    assert!(
        !runtime
            .list_agents()
            .iter()
            .any(|run| run.category.as_deref() == Some("lesson"))
    );
    runtime.stop(target, StopScope::SelfOnly).unwrap();
    runtime.wait(target).await.unwrap();
}

#[tokio::test]
async fn learning_internal_goal_completion_does_not_trigger_learning() {
    let (runtime, model, _) = harness(vec![Step::Stop, Step::Check(true), Step::Stop], vec![]);
    let runtime = enable(runtime, &model);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .create_thread_goal(
            "thread",
            root,
            "Internal work".into(),
            vec!["Verified".into()],
        )
        .unwrap();
    runtime.spawn_reserved(
        root,
        None,
        Role::Worker,
        "Internal work",
        RunConfig {
            learning_internal: true,
            ..Default::default()
        },
    );
    assert_eq!(runtime.wait(root).await.unwrap(), AgentRunPhase::Done);
    assert_eq!(
        runtime.thread_goal("thread").unwrap().phase,
        ThreadGoalPhase::Complete
    );
    assert_eq!(runtime.list_agents().len(), 1);
    assert!(runtime.shared.learning_runs.lock().unwrap().is_empty());
}
