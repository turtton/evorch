// allow: SIZE_OK — tool-call 実行・terminal 網羅 match・標準ツール定義・その unit
// tests が一体の契約 (dispatch seam) を構成する。250 超過の分割 (tool_specs 抽出)
// は後続タスク候補。本タスクの変更前は 249 行で境界線上にあった。
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{Event, LifecycleEvent};
use providers::ToolSpec;
use sandbox::{ApprovalGate, ApprovalOutcome, PolicyDecision};
use serde_json::Value;
use tools::executor::PreparedToolCall;
use tools::{ToolExecutionContext, ToolExecutionMode, ToolResult};

use super::LoopState;
use crate::escalation::detector::ToolObservation;
use crate::network::{NetworkAccessDecision, judge_web_network_access};
use crate::{ExecutionPolicy, META_OPS, is_meta_op, meta, rules};

#[cfg(test)]
mod rework_tests {
    use super::*;
    use crate::{AgentRuntime, EscalationSettings, Role, RunConfig};
    use event_bus::{AgentRunPhase, EventKind, ToolEvent};
    use serde_json::json;

    struct ScriptedModel {
        responses: tokio::sync::Mutex<std::collections::VecDeque<providers::ChatResponse>>,
        observed: tokio::sync::Mutex<Vec<Vec<providers::Message>>>,
    }
    impl ScriptedModel {
        fn new(responses: impl IntoIterator<Item = providers::ChatResponse>) -> Self {
            Self {
                responses: tokio::sync::Mutex::new(responses.into_iter().collect()),
                observed: tokio::sync::Mutex::new(Vec::new()),
            }
        }
        async fn observed(&self) -> Vec<Vec<providers::Message>> {
            self.observed.lock().await.clone()
        }
    }
    #[async_trait::async_trait]
    impl crate::AgentModel for ScriptedModel {
        fn selected_model(&self, _: Role) -> String {
            "test".into()
        }
        async fn complete(
            &self,
            _: &crate::AgentInvocationContext,
            _: Role,
            messages: &[providers::Message],
            _: &[ToolSpec],
        ) -> Result<providers::ChatResponse, crate::RuntimeError> {
            self.observed.lock().await.push(messages.to_vec());
            Ok(self.responses.lock().await.pop_front().expect("response"))
        }
    }
    fn tool_response(id: &str, name: &str, input: Value) -> providers::ChatResponse {
        let mut response = text_response("", providers::FinishReason::ToolUse);
        response.message.content = vec![providers::ContentBlock::ToolUse {
            id: id.into(),
            name: name.into(),
            input,
        }];
        response
    }
    fn text_response(
        text: &str,
        finish_reason: providers::FinishReason,
    ) -> providers::ChatResponse {
        providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text { text: text.into() }],
            },
            finish_reason,
            usage: providers::Usage::default(),
        }
    }
    async fn drain_events(receiver: &mut event_bus::EventReceiver) -> Vec<Event> {
        let mut events = Vec::new();
        while let Ok(Ok(event)) =
            tokio::time::timeout(Duration::from_millis(10), receiver.recv()).await
        {
            events.push(event);
        }
        events
    }

    struct Panics;
    struct Waits;
    #[async_trait::async_trait]
    impl tools::Tool for Waits {
        fn name(&self) -> &'static str {
            "read"
        }
        fn schema(&self) -> Value {
            json!({})
        }
        fn permissions(&self) -> tools::Permissions {
            tools::Permissions::read_only()
        }
        fn execution_mode(&self) -> ToolExecutionMode {
            ToolExecutionMode::Shared
        }
        async fn execute(&self, _: Value) -> Result<ToolResult, tools::ToolError> {
            std::future::pending().await
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn cancellation_while_wave_starts_drains_all_terminal_results() {
        let bus = Arc::new(event_bus::EventBus::new(4096));
        let mut events = bus.subscribe();
        let mut executor = tools::ToolExecutor::new(bus.clone());
        executor.register(Arc::new(Waits)).expect("register");
        let mut batch = tool_response("0", "read", json!({}));
        for index in 1..256 {
            batch.message.content.extend(
                tool_response(&index.to_string(), "read", json!({}))
                    .message
                    .content,
            );
        }
        let model = Arc::new(ScriptedModel::new([batch]));
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model);
        let run = runtime.delegate_background(Role::Worker, "cancel".into(), RunConfig::default());
        loop {
            if matches!(
                events.recv().await.expect("event").kind,
                EventKind::Tool(ToolEvent::ToolStarted { .. })
            ) {
                break;
            }
        }
        runtime.cancel(run).expect("cancel");
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), runtime.wait(run))
                .await
                .expect("drain")
                .expect("wait"),
            AgentRunPhase::Error
        );
        let events = drain_events(&mut events).await;
        let completed: std::collections::HashSet<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Tool(ToolEvent::ToolCompleted {
                    call_id,
                    is_error: true,
                    ..
                }) => Some(call_id),
                _ => None,
            })
            .collect();
        assert_eq!(completed.len(), 256);
    }
    #[async_trait::async_trait]
    impl tools::Tool for Panics {
        fn name(&self) -> &'static str {
            "grep"
        }
        fn schema(&self) -> Value {
            json!({})
        }
        fn permissions(&self) -> tools::Permissions {
            tools::Permissions::read_only()
        }
        fn execution_mode(&self) -> ToolExecutionMode {
            ToolExecutionMode::Shared
        }
        async fn execute(&self, _: Value) -> Result<ToolResult, tools::ToolError> {
            panic!("injected panic")
        }
    }

    async fn scenario(kind: &str) -> (Vec<Event>, Arc<ScriptedModel>, AgentRunPhase) {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir(&root).expect("root");
        let file = root.join("data");
        std::fs::write(&file, "data").expect("file");
        let bus = Arc::new(event_bus::EventBus::new(128));
        let mut events = bus.subscribe();
        let mut executor = tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        executor.register(Arc::new(Panics)).expect("register");
        executor.set_policy(
            sandbox::ApprovalPolicy::allow_all()
                .with_override("shell", sandbox::PolicyDecision::Deny),
        );
        if kind == "terminal" {
            executor.set_policy(
                sandbox::ApprovalPolicy::standard(sandbox::ApprovalMode::OnRequest)
                    .with_override("read", sandbox::PolicyDecision::Ask),
            );
            executor.set_approval_gate(ApprovalGate::new(bus.clone(), Duration::from_millis(10)));
        }
        let mut batch = tool_response("first", "read", json!({"path":file}));
        let second = match kind {
            "panic" => tool_response("second", "grep", json!({})),
            "invalid" => tool_response("second", "edit", json!({})),
            "terminal" => {
                batch = tool_response("finish", "finish", json!({"result":"done"}));
                tool_response("second", "read", json!({"path":file}))
            }
            _ => tool_response("second", "shell", json!({"command":"true"})),
        };
        batch.message.content.extend(second.message.content);
        let model = Arc::new(ScriptedModel::new([
            batch,
            text_response("done", providers::FinishReason::Stop),
        ]));
        let service = Arc::new(
            crate::snapshot::SnapshotService::new(&root, &temp.path().join("snapshots"))
                .expect("snapshot"),
        );
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
            .with_snapshots(service)
            .with_escalation_settings(EscalationSettings {
                consecutive_edit_failures: 1,
                same_file_rewrites: 100,
                tool_call_threshold: 100,
            });
        let role = if kind == "terminal" {
            Role::Orchestrator
        } else {
            Role::Worker
        };
        let run = runtime.delegate_background(role, "test".into(), RunConfig::default());
        let phase = runtime.wait(run).await.expect("wait");
        (drain_events(&mut events).await, model, phase)
    }

    #[tokio::test]
    async fn rejected_preflight_emits_lifecycle_at_original_position() {
        let (events, _, _) = scenario("denied").await;
        let order: Vec<_> = events
            .iter()
            .filter_map(|e| match &e.kind {
                EventKind::Tool(ToolEvent::ToolStarted { call_id, .. }) => {
                    Some(format!("start:{call_id}"))
                }
                EventKind::Tool(ToolEvent::ExecutionDenied { call_id, .. }) => {
                    Some(format!("deny:{call_id}"))
                }
                EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. }) => {
                    Some(format!("end:{call_id}"))
                }
                _ => None,
            })
            .collect();
        assert_eq!(
            order,
            [
                "start:first",
                "end:first",
                "start:second",
                "deny:second",
                "end:second"
            ]
        );
    }

    #[tokio::test]
    async fn invalid_edit_is_observed_without_snapshot() {
        let (events, _, _) = scenario("invalid").await;
        assert!(events.iter().any(|e| matches!(
            e.kind,
            EventKind::Lifecycle(LifecycleEvent::EscalationProposed { .. })
        )));
        assert!(
            !events
                .iter()
                .any(|e| matches!(e.kind, EventKind::Snapshot(_)))
        );
    }

    #[tokio::test]
    async fn panic_preserves_every_wave_result_in_context() {
        let (_, model, phase) = scenario("panic").await;
        assert_eq!(phase, AgentRunPhase::Done);
        let observed = model.observed().await;
        let results: Vec<_> = observed
            .last()
            .expect("request")
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|b| match b {
                providers::ContentBlock::ToolResult {
                    tool_call_id,
                    is_error,
                    ..
                } => Some((tool_call_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(results, [("first", false), ("second", true)]);
    }

    #[tokio::test]
    async fn terminal_barrier_skips_tail_preflight_events() {
        let (events, _, _) = scenario("terminal").await;
        assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. } | ToolEvent::ExecutionDenied { call_id, .. } | ToolEvent::ApprovalRequested { call_id, .. }) if call_id.contains("second"))));
    }
}

/// 承認待ちの上限。TimedOut は error result として run を継続する。
const WEB_APPROVAL_TIMEOUT: Duration = Duration::from_secs(300);

/// [`LoopState::gate_network_tool`] の判定結果。
enum NetworkGate {
    /// ツール実行へ進む。
    Proceed,
    /// 実行せず、エラー結果をツール結果として履歴に積む。
    Reject(ToolResult),
    /// run がキャンセル済み (`finish_cancelled` 呼び出し済み)。
    Cancelled,
}

enum ReadyCall {
    Tool(PreparedToolCall),
    Local,
    Rejected(ToolResult),
    Executed(ToolResult),
    Invalid(tools::ToolError),
}

struct BatchCall {
    id: String,
    name: String,
    input: Value,
    ready: ReadyCall,
}

impl LoopState {
    pub(super) async fn execute_tools(
        &mut self,
        tool_uses: Vec<(String, String, serde_json::Value)>,
    ) -> bool {
        let ctx = ToolExecutionContext {
            run_id: self.task.run_id.to_string(),
        };
        let mut rule_targets = Vec::new();
        let mut remaining = tool_uses.into_iter().peekable();
        while remaining.peek().is_some() {
            let mut segment = Vec::new();
            for call in remaining.by_ref() {
                let terminal = matches!(call.1.as_str(), "finish" | "escalate");
                segment.push(call);
                if terminal {
                    break;
                }
            }
            let mut validated = Vec::with_capacity(segment.len());
            for (id, name, input) in segment {
                if let Some(permit) = &self.task.config.ownership
                    && let Err(error) = permit.validate_mutation()
                {
                    self.finish_error(error.to_string());
                    return false;
                }
                let local = is_meta_op(&name)
                    || matches!(
                        name.as_str(),
                        "task_claim" | "task_complete" | "finding_append"
                    );
                let mut permission = if matches!(
                    name.as_str(),
                    "task_claim" | "task_complete" | "finding_append"
                ) {
                    Ok(())
                } else {
                    self.guard_team_artifact(&name, &input)
                        .and_then(|()| self.policy.authorize(&name).map_err(|e| e.to_string()))
                };
                if local && let Some(spec) = self.tool_specs.iter().find(|spec| spec.name == name) {
                    permission = permission.and_then(|()| {
                        tools::ToolExecutor::validate_schema(&name, &spec.input_schema, &input)
                            .map_err(|error| error.to_string())
                    });
                }
                let validation = if local {
                    None
                } else {
                    Some(self.shared.executor.validate_call(
                        ctx.clone(),
                        name.clone(),
                        id.clone(),
                        input.clone(),
                    ))
                };
                validated.push((id, name, input, permission, validation));
            }
            let mut calls = std::collections::VecDeque::new();
            for (id, name, input, permission, validation) in validated {
                if self.cancelled() {
                    self.finish_cancelled();
                    return false;
                }
                let ready = match permission {
                    Err(error) => ReadyCall::Rejected(ToolResult::error(error)),
                    Ok(()) => match validation {
                        None => ReadyCall::Local,
                        Some(Err(error)) => match self.gate_network_tool(&name, &id).await {
                            NetworkGate::Cancelled => return false,
                            NetworkGate::Reject(result) => ReadyCall::Rejected(result),
                            NetworkGate::Proceed => ReadyCall::Invalid(error),
                        },
                        Some(Ok(call)) => match self.gate_network_tool(&name, &id).await {
                            NetworkGate::Cancelled => return false,
                            NetworkGate::Reject(result) => ReadyCall::Rejected(result),
                            NetworkGate::Proceed => {
                                let mut cancel = self.channels.cancel_rx.clone();
                                let authorized = tokio::select! {
                                    biased;
                                    _ = cancel.wait_for(|cancelled| *cancelled) => { self.finish_cancelled(); return false; }
                                    result = call.authorize() => result,
                                };
                                match authorized {
                                    Ok(call) => ReadyCall::Tool(call),
                                    Err(error) => ReadyCall::Invalid(error),
                                }
                            }
                        },
                    },
                };
                calls.push_back(BatchCall {
                    id,
                    name,
                    input,
                    ready,
                });
            }
            while let Some(first) = calls.pop_front() {
                let mut wave = vec![first];
                if self.shared_call(&wave[0]) {
                    while calls.front().is_some_and(|call| self.shared_call(call)) {
                        if let Some(call) = calls.pop_front() {
                            wave.push(call);
                        }
                    }
                }
                let mut completed = Vec::new();
                let mut tasks = tokio::task::JoinSet::new();
                let mut pending = std::collections::HashMap::new();
                let mut prepared_wave = Vec::with_capacity(wave.len());
                for (index, call) in wave.into_iter().enumerate() {
                    if let Some(permit) = &self.task.config.ownership
                        && let Err(error) = permit.validate_mutation()
                    {
                        self.finish_error(error.to_string());
                        return false;
                    }
                    if self.cancelled() {
                        self.finish_cancelled();
                        return false;
                    }
                    let BatchCall {
                        id,
                        name,
                        input,
                        ready,
                    } = call;
                    match ready {
                        ReadyCall::Tool(call) => {
                            let guard = match self.snapshot_before_tool(&name, &id).await {
                                Ok(guard) => guard,
                                Err(error) => {
                                    completed.push((
                                        index,
                                        id,
                                        name,
                                        input,
                                        ReadyCall::Rejected(ToolResult::error(error)),
                                    ));
                                    continue;
                                }
                            };
                            if let Some(permit) = &self.task.config.ownership
                                && let Err(error) = permit.validate_mutation()
                            {
                                self.finish_error(error.to_string());
                                return false;
                            }
                            prepared_wave.push((index, id, name, input, call, guard));
                        }
                        ReadyCall::Invalid(error) => {
                            completed.push((index, id, name, input, ReadyCall::Invalid(error)));
                        }
                        ready => completed.push((index, id, name, input, ready)),
                    }
                }
                for (index, id, name, input, call, guard) in prepared_wave {
                    let metadata = (index, id.clone(), name.clone(), input.clone());
                    let mut cancel = self.channels.cancel_rx.clone();
                    let bus = Arc::clone(&self.shared.bus);
                    let run_id = ctx.run_id.clone();
                    let handle = tasks.spawn(async move {
                            let _guard = guard;
                            let result = tokio::select! {
                                biased;
                                _ = cancel.wait_for(|cancelled| *cancelled) => {
                                    bus.emit(Event::new(event_bus::ToolEvent::ToolCompleted {
                                        tool_name: name.clone(), call_id: id.clone(), is_error: true,
                                        output: Some("cancelled".into()), detail: None, run_id: Some(run_id),
                                    }));
                                    ToolResult::error("cancelled")
                                },
                                result = call.execute() => result.unwrap_or_else(|error| ToolResult::error(error.to_string())),
                            };
                            (index, id, name, input, ReadyCall::Executed(result))
                        });
                    pending.insert(handle.id(), metadata);
                }
                while let Some(result) = tasks.join_next_with_id().await {
                    match result {
                        Ok((task_id, result)) => {
                            pending.remove(&task_id);
                            completed.push(result);
                        }
                        Err(error) => {
                            if let Some((index, id, name, input)) = pending.remove(&error.id()) {
                                let result = ToolResult::error(error.to_string());
                                self.shared.bus.emit(Event::new(
                                    event_bus::ToolEvent::ToolCompleted {
                                        tool_name: name.clone(),
                                        call_id: id.clone(),
                                        is_error: true,
                                        output: Some(result.content.clone()),
                                        detail: None,
                                        run_id: Some(ctx.run_id.clone()),
                                    },
                                ));
                                completed.push((
                                    index,
                                    id,
                                    name,
                                    input,
                                    ReadyCall::Executed(result),
                                ));
                            }
                        }
                    }
                }
                completed.sort_by_key(|(index, ..)| *index);
                for (_, id, name, input, ready) in completed {
                    if matches!(&ready, ReadyCall::Local)
                        && let Some(permit) = &self.task.config.ownership
                        && let Err(error) = permit.validate_mutation()
                    {
                        self.finish_error(error.to_string());
                        return false;
                    }
                    let observed = matches!(&ready, ReadyCall::Executed(_) | ReadyCall::Invalid(_));
                    let result =
                        if let ReadyCall::Rejected(result) | ReadyCall::Executed(result) = ready {
                            result
                        } else if let ReadyCall::Invalid(error) = ready {
                            self.shared.executor.report_invalid_call(
                                &ctx,
                                &name,
                                &id,
                                input.clone(),
                                &error,
                            );
                            ToolResult::error(error.to_string())
                        } else if matches!(
                            name.as_str(),
                            "task_claim" | "task_complete" | "finding_append"
                        ) {
                            self.team_tool(&name, input.clone()).await
                        } else if let Err(error) = self.guard_team_artifact(&name, &input) {
                            ToolResult::error(error)
                        } else if let Err(error) = self.policy.authorize(&name) {
                            ToolResult::error(error.to_string())
                        } else if is_meta_op(&name) {
                            let dispatch = meta::dispatch(self, &name, input).await;
                            self.context.push_tool_result(id, dispatch.result);
                            self.publish_message_count();
                            match dispatch.terminal {
                                meta::Terminal::Continue => continue,
                                meta::Terminal::Finish(result) => {
                                    self.push_final_result(&result);
                                    self.finish_success();
                                    return false;
                                }
                                meta::Terminal::Escalate(memo) => {
                                    // 終端指示を返した時点で残りのバッチ tool call は
                                    // 実行しない (新規 tool call 受付の停止)。
                                    self.finish_escalated(*memo);
                                    return false;
                                }
                            }
                        } else {
                            ToolResult::error("invalid prepared local call")
                        };
                    let rule_target = matches!(name.as_str(), "read" | "edit" | "grep")
                        .then(|| input.get("path").and_then(Value::as_str).map(Into::into))
                        .flatten();
                    // 停滞検出は観測専用。提案は履歴へ注入せず EscalationProposed
                    // イベントの発行だけを行う (メタ操作分岐は観測対象外)。
                    let observation_path = if name == "edit" {
                        rule_target.as_deref().map(PathBuf::from)
                    } else {
                        None
                    };
                    if observed
                        && let Some(trigger) = self.escalation_detector.observe(
                            &ToolObservation {
                                tool: name.as_str(),
                                path: observation_path,
                                is_error: result.is_error,
                            },
                            &self.shared.escalation,
                        )
                    {
                        self.shared
                            .bus
                            .emit(Event::new(LifecycleEvent::EscalationProposed {
                                run_id: self.caller_run_id().to_string(),
                                trigger,
                            }));
                    }
                    if observed
                        && !result.is_error
                        && let Some(target) = rule_target
                    {
                        rule_targets.push(target);
                    }
                    self.context.push_tool_result(id, result);
                    self.publish_message_count();
                }
                if self.cancelled() {
                    self.finish_cancelled();
                    return false;
                }
                if let Some(permit) = &self.task.config.ownership
                    && let Err(error) = permit.validate_mutation()
                {
                    self.finish_error(error.to_string());
                    return false;
                }
            }
        }
        if !rule_targets.is_empty()
            && let Some(session) = &mut self.rules_session
            && let Some(text) = rules::after_successful_tools(session, &rule_targets)
        {
            self.context.push_user(&text);
            self.publish_message_count();
        }
        true
    }

    fn shared_call(&self, call: &BatchCall) -> bool {
        !is_meta_op(&call.name)
            && !matches!(
                call.name.as_str(),
                "task_claim" | "task_complete" | "finding_append"
            )
            && !self
                .shared
                .executor
                .tool_permissions(&call.name)
                .is_some_and(|permissions| permissions.fs_write)
            && self.shared.executor.tool_execution_mode(&call.name) == ToolExecutionMode::Shared
    }

    /// network 権限を持つツールに 3 層 AND 判定 (role / per-tool / session) を適用する。
    /// 非 network ツール・未登録ツールはそのまま通す (UnknownTool は executor 側で処理)。
    /// Ask は ApprovalGate (EventBus ApprovalRequested/ApprovalResolved) で 1 回だけ承認を求める。
    /// 承認相関キーは `{run_id}:{call_id}` に run スコープ化する (call_id は
    /// model 由来で run-local のため、同一 EventBus 上の並列 run と衝突しうる)。
    async fn gate_network_tool(&mut self, name: &str, call_id: &str) -> NetworkGate {
        let Some(permissions) = self.shared.executor.tool_permissions(name) else {
            return NetworkGate::Proceed;
        };
        if !permissions.network {
            return NetworkGate::Proceed;
        }
        let per_tool = self
            .shared
            .executor
            .classify_tool(name)
            .unwrap_or(PolicyDecision::Deny);
        match judge_web_network_access(
            &self.policy.capabilities,
            &self.policy.role_name,
            name,
            per_tool,
            self.task.config.network_access,
        ) {
            NetworkAccessDecision::Allow => NetworkGate::Proceed,
            NetworkAccessDecision::Deny { reason } => {
                NetworkGate::Reject(ToolResult::error(reason))
            }
            NetworkAccessDecision::Ask { reason } => {
                let gate = ApprovalGate::new(Arc::clone(&self.shared.bus), WEB_APPROVAL_TIMEOUT);
                // 承認相関キーは run スコープ化する: 同一 EventBus 上の並列 run が
                // 同一 call_id (model 由来で run-local) を使いうるため、run_id を
                // 前置して他 run 宛ての ApprovalResolved を受け付けない。
                let correlation_id = format!("{}:{}", self.task.run_id, call_id);
                let outcome = tokio::select! {
                    biased;
                    changed = self.channels.cancel_rx.changed() => {
                        if changed.is_ok() && self.cancelled() {
                            self.finish_cancelled();
                            return NetworkGate::Cancelled;
                        }
                        // executor 実行の select と同じガードだが、承認待ちを破棄した
                        // 後に無承認で実行されないよう fail-closed で拒否する。
                        return NetworkGate::Reject(ToolResult::error(
                            "cancel 監視が変化したため承認待ちを中止しました",
                        ));
                    }
                    outcome = gate.request(name, &correlation_id) => outcome,
                };
                match outcome {
                    ApprovalOutcome::Approved => NetworkGate::Proceed,
                    ApprovalOutcome::Denied => NetworkGate::Reject(ToolResult::error(format!(
                        "承認要求が拒否されました: {reason}"
                    ))),
                    ApprovalOutcome::TimedOut => NetworkGate::Reject(ToolResult::error(format!(
                        "承認応答がタイムアウトしました: {reason}"
                    ))),
                }
            }
        }
    }
}

/// 標準ツール定義を返す。
/// Web ツールの露出ゲートは [`ExecutionPolicy::filter_tool_specs`] が担い、
/// 実行時には network 権限ツールへの 3 層 AND 判定 (role / per-tool / session、
/// session OptIn は承認プロンプト) が execute_tools の network gate で行われる。
pub(super) fn standard_tool_specs() -> Vec<ToolSpec> {
    [
        "read",
        "edit",
        "grep",
        "shell",
        "git_diff",
        "web_search",
        "web_fetch",
    ]
    .into_iter()
    .chain(META_OPS.iter().copied())
    .map(|name| ToolSpec {
        name: name.to_string(),
        description: format!("{name} tool"),
        input_schema: serde_json::json!({ "type": "object" }),
    })
    .collect()
}

/// モデルに見せるツール定義を決定する。
///
/// role の capability filter を適用した上で、skill レジストリが未接続の
/// ランタイムからは `skill_load` を除く。`skill_load` は capability 上
/// Orchestrator/Worker に許可されているが、レジストリなしでは呼び出しが
/// 必ず失敗するため、失敗前提の定義をモデルに見せない (model only sees
/// tools that can work)。
pub(super) fn visible_tool_specs(
    specs: Vec<ToolSpec>,
    policy: &ExecutionPolicy,
    skills_configured: bool,
) -> Vec<ToolSpec> {
    policy
        .filter_tool_specs(specs)
        .into_iter()
        .filter(|spec| skills_configured || spec.name != "skill_load")
        .collect()
}

#[cfg(test)]
mod tests {
    use agents::Role;

    use super::*;
    use crate::ExecutionPolicy;

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.name.as_str()).collect()
    }

    // Given: 標準ツール定義
    // When: standard_tool_specs を呼ぶ
    // Then: web_search と web_fetch の定義が含まれる
    #[test]
    fn standard_tool_specs_include_web_search_and_web_fetch() {
        let specs = standard_tool_specs();
        let tool_names = names(&specs);

        assert!(tool_names.contains(&"web_search"));
        assert!(tool_names.contains(&"web_fetch"));
    }

    // Given: Librarian のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: web_search と web_fetch がモデルに見える
    #[test]
    fn visible_tool_specs_exposes_both_web_tools_for_librarian() {
        let policy = ExecutionPolicy::for_role(Role::Librarian);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false);
        let tool_names = names(&specs);

        assert!(tool_names.contains(&"web_search"));
        assert!(tool_names.contains(&"web_fetch"));
    }

    // Given: Orchestrator のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: web_fetch のみがモデルに見える
    #[test]
    fn visible_tool_specs_exposes_only_web_fetch_for_orchestrator() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false);
        let tool_names = names(&specs);

        assert!(tool_names.contains(&"web_fetch"));
        assert!(!tool_names.contains(&"web_search"));
    }

    // Given: Explorer、Worker、Reviewer のポリシーと skills 未設定
    // When: 各ロールで visible_tool_specs を呼ぶ
    // Then: web_search と web_fetch はモデルに見えない
    #[test]
    fn visible_tool_specs_hides_web_tools_for_explorer_worker_reviewer() {
        for role in [Role::Explorer, Role::Worker, Role::Reviewer] {
            let policy = ExecutionPolicy::for_role(role);

            let specs = visible_tool_specs(standard_tool_specs(), &policy, false);
            let tool_names = names(&specs);

            assert!(!tool_names.contains(&"web_search"));
            assert!(!tool_names.contains(&"web_fetch"));
        }
    }

    // Given: Worker のポリシー (skill_load は capability 内) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load はモデルに見せる定義に残る
    #[test]
    fn visible_tool_specs_keeps_skill_load_for_worker_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true);

        assert!(names(&specs).contains(&"skill_load"));
    }

    // Given: Worker のポリシー
    // When: visible_tool_specs を呼ぶ
    // Then: escalate がモデルに見える
    #[test]
    fn visible_tool_specs_exposes_escalate_for_worker() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false);

        assert!(names(&specs).contains(&"escalate"));
    }

    // Given: Worker のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load は除去され、capability 内の通常ツールは保持される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_worker_when_skills_not_configured() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false);

        assert!(!names(&specs).contains(&"skill_load"));
        assert!(names(&specs).contains(&"edit"));
    }

    // Given: Explorer のポリシー (skill_load は capability 外) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: capability filter により skill_load は除去される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_explorer_even_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Explorer);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true);

        assert!(!names(&specs).contains(&"skill_load"));
    }

    // Given: Orchestrator のポリシー (skill_load は capability 内) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load はモデルに見せる定義に残る
    #[test]
    fn visible_tool_specs_keeps_skill_load_for_orchestrator_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true);

        assert!(names(&specs).contains(&"skill_load"));
    }

    // Given: Orchestrator のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: capability 内でも skill_load は除去される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_orchestrator_when_skills_not_configured() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false);

        assert!(!names(&specs).contains(&"skill_load"));
    }
}
