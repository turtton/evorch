use super::*;

#[tokio::test]
async fn budget_overrun_emits_budget_exhausted_once() {
    // Given: a five-call budget, independent of reread and progress limits.
    let config = RunConfig {
        budget: runtime::budget_tracker::BudgetSettings {
            max_tool_calls: 5,
            ..Default::default()
        },
        ..Default::default()
    };
    // When: eight calls are requested.
    let events = run_calls(8, config).await;
    let completed: Vec<_> = events
        .iter()
        .filter_map(|event| match &event.kind {
            EventKind::Tool(event_bus::ToolEvent::ToolCompleted { call_id, .. }) => {
                Some(call_id.as_str())
            }
            _ => None,
        })
        .collect();
    assert_eq!(completed, ["0", "1", "2", "3", "4"]);
    assert!(events.iter().any(|event| matches!(&event.kind,
        EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {to: AgentRunPhase::Error, reason: Some(reason), ..})
        if reason.starts_with("BudgetExhausted:"))));
    let progress = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Orchestrator(OrchestratorEvent::TaskProgressed { progress, .. })
                if progress.get("status").and_then(serde_json::Value::as_str) == Some("failed") =>
            {
                Some(progress)
            }
            _ => None,
        })
        .expect("persisted exhaustion");
    let task: storage::entity::TaskContinuation =
        serde_json::from_value(progress.clone()).expect("typed continuation");
    let cursor: Vec<providers::Message> =
        serde_json::from_str(task.resume_cursor.as_deref().expect("cursor")).expect("messages");
    let results: Vec<_> = cursor
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            providers::ContentBlock::ToolResult { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(results, ["0", "1", "2", "3", "4"]);
    let breach = events
        .iter()
        .find_map(|event| match &event.kind {
            EventKind::Diagnostic(diagnostic) if diagnostic.code == "BudgetExhausted" => {
                Some(diagnostic)
            }
            _ => None,
        })
        .expect("breach diagnostic");
    assert_eq!(
        task.failure_reason,
        Some(format!("{}: {}", breach.code, breach.detail))
    );
    // Then: the threshold is latched for this run.
    assert_eq!(
        diagnostics(
            &events,
            event_bus::event::diagnostic_codes::BUDGET_EXHAUSTED
        ),
        1
    );
}

pub(super) async fn run_calls(count: u32, config: RunConfig) -> Vec<Event> {
    run_calls_in_batches(count, config, false).await
}

pub(super) async fn run_calls_in_batches(
    count: u32,
    mut config: RunConfig,
    batch: bool,
) -> Vec<Event> {
    // Isolate budget/checkpoint contracts from the independent identical-call guard.
    config.budget.max_identical_tool_call_repeats = count.saturating_add(1);
    // durable 境界イベントは task 識別子を持たない run では発行されないため、
    // checkpoint を観測する fixture には明示的な identity を持たせる。
    if config.task_id.is_none() && config.team_task.is_none() {
        config.task_id = Some("budget-checkpoint".into());
    }
    let file = tempfile::NamedTempFile::new().expect("file");
    std::fs::write(file.path(), "budget fixture").expect("write");
    let mut script = Vec::new();
    for index in 0..count {
        let mut response = tool_response(
            &index.to_string(),
            "read",
            serde_json::json!({"path": file.path()}),
        );
        response.usage.input_tokens = 3;
        response.usage.output_tokens = 2;
        script.push(Ok(response));
    }
    if batch {
        let content = script
            .iter()
            .flat_map(|response| response.as_ref().expect("response").message.content.clone())
            .collect();
        let mut response = text_response("", FinishReason::ToolUse);
        response.message.content = content;
        script = vec![Ok(response)];
    }
    script.push(Ok(text_response("done", FinishReason::Stop)));
    let bus = Arc::new(EventBus::new(4096));
    let mut receiver = bus.subscribe();
    let executor = tools::ToolExecutor::with_standard_tools(
        bus.clone(),
        Arc::new(sandbox::DirectSandbox::new_unchecked()),
    );
    let model = Arc::new(ScriptedModel::new(script));
    let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
    let run = runtime.delegate_background(Role::Worker, "budget".into(), config);
    // 完了検知はランタイムの wait に委譲し、テスト内には壁時計デッドラインを持たない。
    // 固定 timeout は共有 CI ランナーの負荷変動で正当な実行時間を超過し flaky になる
    // (実績: 正常時 2.5s / 負荷時 10.3s)。ハング検知は nextest の
    // slow-timeout / terminate-after (.config/nextest.toml) が担う。
    let phase = runtime.wait(run).await.expect("wait");
    let mut events = Vec::new();
    loop {
        let event = receiver.recv().await.expect("event");
        let terminal = matches!(
            &event.kind,
            EventKind::Lifecycle(event_bus::LifecycleEvent::AgentRunStateChanged {
                to: AgentRunPhase::Done | AgentRunPhase::Error,
                ..
            })
        );
        events.push(event);
        if terminal {
            break;
        }
    }
    assert_eq!(
        phase,
        if events.iter().any(
            |event| matches!(&event.kind, EventKind::Diagnostic(d) if d.source == "budget_tracker" && d.severity == event_bus::DiagnosticSeverity::Error)
        ) {
            AgentRunPhase::Error
        } else {
            AgentRunPhase::Done
        }
    );
    let tool_limit = events.iter().any(|event| matches!(&event.kind, EventKind::Diagnostic(d) if d.source == "budget_tracker" && d.detail.contains("max_tool_calls=5")));
    if tool_limit {
        assert_eq!(model.observed().await.len(), if batch { 1 } else { 5 });
    }
    events
}
