// allow: SIZE_OK — tool-call 実行・terminal 網羅 match・標準ツール定義・その unit
// tests が一体の契約 (dispatch seam) を構成する。250 超過の分割 (tool_specs 抽出)
// は後続タスク候補。本タスクの変更前は 249 行で境界線上にあった。
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use event_bus::{
    DiagnosticEvent, DiagnosticSeverity, Event, LifecycleEvent, event::diagnostic_codes,
};
use providers::ToolSpec;
use sandbox::{ApprovalGate, ApprovalOutcome, PolicyDecision};
use serde_json::Value;
use tools::executor::PreparedToolCall;
use tools::{ToolExecutionContext, ToolExecutionMode, ToolResult};

use super::{LoopState, run_js_host};
use crate::escalation::detector::{
    EscalationDetector, ToolErrorClass, ToolObservation, patch_rewrite_paths,
};
use crate::network::{NetworkAccessDecision, judge_web_network_access};
use crate::{ExecutionPolicy, InterruptKind, META_OPS, is_meta_op, meta, rules};

mod ledger_events;
mod mcp_scope;

#[cfg(test)]
mod finish_tests;
#[cfg(test)]
mod network_review_tests;

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
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
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
        let run = runtime.delegate_background(
            Role::Worker,
            "cancel".into(),
            RunConfig {
                budget: crate::budget_tracker::BudgetSettings {
                    max_identical_tool_call_repeats: 257,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
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
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
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
    async fn invalid_edit_is_observed_by_escalation() {
        let (events, _, _) = scenario("invalid").await;
        assert!(events.iter().any(|e| matches!(
            e.kind,
            EventKind::Lifecycle(LifecycleEvent::EscalationProposed { .. })
        )));
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
    async fn file_escalation_denial_reaches_agent_once_without_polluting_events() {
        struct Deny;
        #[async_trait::async_trait]
        impl tools::tools::shell_escalation::ShellEscalationGate for Deny {
            async fn decide(
                &self,
                _: &ToolExecutionContext,
                _: &str,
                _: &str,
            ) -> tools::tools::shell_escalation::EscalationDecision {
                unreachable!("only file access")
            }
            async fn decide_file_access(
                &self,
                _: &ToolExecutionContext,
                _: &tools::tools::shell_escalation::FileAccessRequest,
            ) -> tools::tools::shell_escalation::EscalationDecision {
                tools::tools::shell_escalation::EscalationDecision::Deny {
                    reason: "policy denial".into(),
                }
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(128));
        let mut events = bus.subscribe();
        let executor = tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        executor.set_file_boundary(&sandbox::SandboxBoundary::new(project));
        executor.set_shell_escalation(
            Arc::new(Deny),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        let model = Arc::new(ScriptedModel::new([
            tool_response(
                "denied",
                "read",
                json!({"path":temp.path().join("outside"), "sandbox_access":"unsandboxed", "justification":"inspect"}),
            ),
            text_response("done", providers::FinishReason::Stop),
        ]));
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone());
        let run = runtime.delegate_background(Role::Worker, "inspect".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
        let observed = model.observed().await;
        let results: Vec<_> = observed
            .last()
            .unwrap()
            .iter()
            .flat_map(|m| &m.content)
            .filter_map(|block| match block {
                providers::ContentBlock::ToolResult {
                    content, is_error, ..
                } => {
                    assert!(*is_error);
                    Some(
                        content
                            .iter()
                            .map(|part| part.text_or_image_note())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    )
                }
                _ => None,
            })
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("file access escalation denied: policy denial"));
        assert_eq!(
            results[0]
                .matches(crate::escalation_review::DENIAL_GUIDANCE)
                .count(),
            1
        );
        let mut denied = 0;
        for event in events.drain_pending_snapshot() {
            match event.kind {
                EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) => {
                    denied += 1;
                    assert_eq!(reason, "file access escalation denied: policy denial");
                }
                EventKind::Diagnostic(diagnostic) => assert!(
                    !diagnostic
                        .detail
                        .contains(crate::escalation_review::DENIAL_GUIDANCE)
                ),
                _ => {}
            }
        }
        assert_eq!(denied, 1);
    }

    #[tokio::test]
    async fn terminal_barrier_skips_tail_preflight_events() {
        let (events, _, _) = scenario("terminal").await;
        assert!(!events.iter().any(|e| matches!(&e.kind, EventKind::Tool(ToolEvent::ToolCompleted { call_id, .. } | ToolEvent::ExecutionDenied { call_id, .. } | ToolEvent::ApprovalRequested { call_id, .. }) if call_id.contains("second"))));
    }

    /// A Worker runtime whose standard tools apply patches inside `root` without
    /// asking, scripted to make `calls` in one batch each and then stop.
    fn patch_runtime(
        root: &std::path::Path,
        calls: Vec<providers::ChatResponse>,
        escalation: EscalationSettings,
    ) -> (AgentRuntime, Arc<ScriptedModel>, event_bus::EventReceiver) {
        let bus = Arc::new(event_bus::EventBus::new(256));
        let events = bus.subscribe();
        let mut executor = tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        executor.set_policy(sandbox::ApprovalPolicy::allow_all());
        executor.set_default_cwd(root.to_path_buf());
        let model = Arc::new(ScriptedModel::new(
            calls
                .into_iter()
                .chain([text_response("done", providers::FinishReason::Stop)]),
        ));
        let runtime = AgentRuntime::new(bus, Arc::new(executor), model.clone())
            .with_escalation_settings(escalation);
        (runtime, model, events)
    }

    fn patch_call(id: &str, patch: &str) -> providers::ChatResponse {
        tool_response(id, "apply_patch", json!({ "input": patch }))
    }

    fn proposals(events: &[Event]) -> Vec<event_bus::EscalationTrigger> {
        events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Lifecycle(LifecycleEvent::EscalationProposed { trigger, .. }) => {
                    Some(trigger.clone())
                }
                _ => None,
            })
            .collect()
    }

    // Given: rules in three directories and a patch that moves a file out of the
    //        first into the second and deletes one in the third
    // When: the patch succeeds in a Worker run
    // Then: the rules of all three directories reach the next request, so every
    //       path the patch names, move source and deletion included, selected rules
    #[tokio::test]
    async fn successful_apply_patch_selects_rules_for_every_named_path() {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("project");
        for (directory, marker) in [("a", "RULE-A"), ("b", "RULE-B"), ("c", "RULE-C")] {
            std::fs::create_dir_all(root.join(directory)).expect("directory");
            std::fs::write(root.join(directory).join("AGENTS.md"), marker).expect("rules");
        }
        std::fs::write(root.join("a/old.rs"), "old\n").expect("source");
        std::fs::write(root.join("c/gone.rs"), "gone\n").expect("deleted");
        let (runtime, model, _events) = patch_runtime(
            &root,
            vec![patch_call(
                "patch",
                "*** Begin Patch\n*** Update File: a/old.rs\n*** Move to: b/new.rs\n@@\n-old\n+new\n*** Delete File: c/gone.rs\n*** End Patch",
            )],
            EscalationSettings::default(),
        );
        let runtime = runtime.with_project_rules(Arc::new(crate::RulesSource::new(
            crate::ProjectTrust::Approved,
            crate::RulesSettings {
                context_window_tokens: 200_000,
                response_headroom_tokens: 16_384,
                max_injection_bytes: 65_536,
            },
            None,
            Some(root.clone()),
            None,
        )));

        let run = runtime.delegate_background(Role::Worker, "patch".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);

        assert_eq!(
            std::fs::read_to_string(root.join("b/new.rs")).expect("moved"),
            "new\n"
        );
        let observed = model.observed().await;
        let texts = |messages: &[providers::Message]| -> String {
            messages
                .iter()
                .flat_map(|message| &message.content)
                .filter_map(|block| match block {
                    providers::ContentBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect()
        };
        let before = texts(&observed[0]);
        let after = texts(observed.last().expect("request"));
        for marker in ["RULE-A", "RULE-B", "RULE-C"] {
            assert!(!before.contains(marker), "{marker} before the patch");
            assert!(after.contains(marker), "{marker} after the patch: {after}");
        }
    }

    // Given: a threshold of two consecutive edit failures
    // When: a Worker sends a patch that does not parse and then one whose
    //       context does not match the file
    // Then: both count as edit failures, and the no-progress evidence names the
    //       patch's own failure class instead of guessing from its text
    #[tokio::test]
    async fn failing_patches_propose_consecutive_edit_failures_with_their_class() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("lib.rs"), "actual\n").expect("file");
        let (runtime, _model, mut events) = patch_runtime(
            temp.path(),
            vec![
                patch_call("parse", "garbage"),
                patch_call(
                    "match",
                    "*** Begin Patch\n*** Update File: lib.rs\n@@\n-expected\n+new\n*** End Patch",
                ),
            ],
            EscalationSettings {
                consecutive_edit_failures: 2,
                same_file_rewrites: 1,
                tool_call_threshold: 100,
            },
        );

        let run = runtime.delegate_background(Role::Worker, "patch".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);

        let events = drain_events(&mut events).await;
        assert_eq!(
            proposals(&events),
            [event_bus::EscalationTrigger::ConsecutiveEditFailures { count: 2 }],
            "{events:?}"
        );
        let detail = events
            .iter()
            .find_map(|event| match &event.kind {
                EventKind::Diagnostic(diagnostic)
                    if diagnostic.code == diagnostic_codes::NO_PROGRESS =>
                {
                    Some(diagnostic.detail.clone())
                }
                _ => None,
            })
            .expect("no-progress diagnostic");
        assert!(
            detail.lines().any(|line| line == "tool=apply_patch"),
            "{detail}"
        );
        assert!(
            detail
                .lines()
                .any(|line| line == "error=edit_target_not_found"),
            "{detail}"
        );
        assert_eq!(
            std::fs::read_to_string(temp.path().join("lib.rs")).expect("file"),
            "actual\n"
        );
    }

    // Given: a threshold of two rewrites of one file
    // When: a Worker patches the same relative path twice, the first time naming
    //       it twice in one patch
    // Then: the second call proposes a repeated rewrite of the file resolved in
    //       the workspace
    #[tokio::test]
    async fn patching_one_file_twice_proposes_a_repeated_rewrite() {
        let temp = tempfile::tempdir().expect("temp");
        std::fs::write(temp.path().join("lib.rs"), "one\n").expect("file");
        let (runtime, _model, mut events) = patch_runtime(
            temp.path(),
            vec![
                patch_call(
                    "first",
                    "*** Begin Patch\n*** Update File: lib.rs\n@@\n-one\n+two\n*** Update File: ./lib.rs\n@@\n-two\n+three\n*** End Patch",
                ),
                patch_call(
                    "second",
                    "*** Begin Patch\n*** Update File: lib.rs\n@@\n-three\n+four\n*** End Patch",
                ),
            ],
            EscalationSettings {
                consecutive_edit_failures: 100,
                same_file_rewrites: 2,
                tool_call_threshold: 100,
            },
        );

        let run = runtime.delegate_background(Role::Worker, "patch".into(), RunConfig::default());
        assert_eq!(runtime.wait(run).await.expect("wait"), AgentRunPhase::Done);

        let events = drain_events(&mut events).await;
        let completed: Vec<_> = events
            .iter()
            .filter_map(|event| match &event.kind {
                EventKind::Tool(ToolEvent::ToolCompleted {
                    call_id, is_error, ..
                }) => Some((call_id.as_str(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(completed, [("first", false), ("second", false)]);
        assert_eq!(
            proposals(&events),
            [event_bus::EscalationTrigger::RepeatedFileRewrite {
                path: temp.path().join("lib.rs").to_string_lossy().into_owned(),
                count: 2,
            }],
            "{events:?}"
        );
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

/// Returned for a tool call whose arguments were not valid JSON; the tool is not run.
pub(crate) const MALFORMED_ARGUMENTS: &str = "The tool call arguments were not valid JSON, so the tool was not run. Call it again with a JSON object that matches the tool's input schema.";

enum ReadyCall {
    Tool(PreparedToolCall),
    Local,
    Rejected(ToolResult),
    Executed(ToolResult),
    /// The executor returned a typed error; kept typed so diagnostics can classify it
    /// without reading the error text, which may echo invalid input values.
    Failed(tools::ToolError),
    Invalid(tools::ToolError),
}

struct BatchCall {
    id: String,
    name: String,
    input: Value,
    ready: ReadyCall,
}

impl BatchCall {
    fn awaited_delegate(&self) -> bool {
        self.name == "delegate"
            && matches!(self.ready, ReadyCall::Local)
            && self
                .input
                .get("background")
                .is_none_or(|value| value == &Value::Bool(false))
    }
}

impl LoopState {
    pub(super) async fn finish_tool_batch(
        &mut self,
        result: &str,
        remaining: impl Iterator<Item = (String, String, Value)>,
    ) -> bool {
        // A new answer or goal can keep this run alive after finish dispatch.
        // Close the unexecuted tail before any final text or follow-up input.
        for (id, _, _) in remaining {
            self.context.push_tool_result(
                id,
                ToolResult::error("Not executed because an earlier finish call ended this tool batch. Submit a new call if the work is still needed."),
            );
        }
        self.push_final_result(result);
        // A shell handoff belongs to this completed batch even when finish is
        // in the same response and this run has no thread goal. Resolve its cut
        // before considering normal completion; never drop reserved host input.
        if self.flush_scheduled_user_messages() || self.has_pending_user_messages() {
            return true;
        }
        if self
            .runtime()
            .is_some_and(|runtime| runtime.goal_for_root(self.task.run_id).is_some())
        {
            if self.flush_aside() || self.thread_goal_boundary().await {
                return true;
            }
            if let Some(kind) = self.interrupted() {
                self.finish_interrupted(kind);
                return false;
            }
            if self.flush_aside() {
                return true;
            }
            if self.task.config.keep_alive {
                return self.wait_for_input().await;
            }
        }
        !self.finish_success()
    }

    pub(super) async fn execute_tools(
        &mut self,
        tool_uses: Vec<(String, String, serde_json::Value)>,
        invocation: &crate::AgentInvocationContext,
        invocation_model: &dyn crate::AgentModel,
    ) -> bool {
        if let Err(error) = self.benchmark_tools_supported(&tool_uses) {
            self.finish_error(error.to_string());
            return false;
        }
        let ctx = ToolExecutionContext {
            run_id: self.task.run_id.to_string(),
            // THREAD_ID_SEAM: RunTask currently carries run identity only.
            thread_id: None,
            call_id: None,
            shell_wait_policy: tools::executor::ShellWaitPolicy::AutoYield,
            shell_input: Some(Arc::new(crate::runtime::user_inbox::RunShellInput {
                inbox: self.channels.user_inbox.clone(),
                mailbox: Arc::clone(&self.task.mailbox),
                parent: self.task.parent.map(|parent| parent.to_string()),
            })),
            parent_call_id: None,
            run_js_host: None,
        };
        let ctx = ToolExecutionContext {
            run_js_host: self.run_js_host(&ctx),
            ..ctx
        };
        let mut rule_targets = Vec::new();
        let mut shell_cwds = Vec::new();
        let mut remaining = tool_uses.into_iter().peekable();
        while remaining.peek().is_some() {
            match self.publish_budget() {
                crate::budget_tracker::BudgetDecision::Continue => {}
                crate::budget_tracker::BudgetDecision::Exhausted(_) => return false,
            }
            let mut segment = Vec::new();
            let capacity = self
                .task
                .config
                .budget
                .max_tool_calls
                .saturating_sub(self.escalation_detector.tool_calls());
            for call in remaining
                .by_ref()
                .take(usize::try_from(capacity).unwrap_or(usize::MAX))
            {
                let terminal = matches!(call.1.as_str(), "finish" | "escalate");
                segment.push(call);
                if terminal {
                    break;
                }
            }
            let mut validated = Vec::with_capacity(segment.len());
            for (id, name, mut input) in segment {
                meta::drop_ignored_arguments(&name, &mut input);
                if let Err(error) = self.validate_owner_mutation().await {
                    self.finish_owner_error("validate", error);
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
                    self.guard_team_artifact(&name, &input).and_then(|()| {
                        if self.shared.executor.requires_scope_gate(&name) {
                            Ok(())
                        } else {
                            self.policy.authorize(&name).map_err(|e| e.to_string())
                        }
                    })
                };
                if name == tools::tools::run_js::NAME && !self.run_js_offered() {
                    permission = permission
                        .and_then(|()| Err("run_js is not enabled for this run".to_owned()));
                }
                // providers::StreamAccumulator turns unparsable arguments into null; tools
                // only take objects, so name the cause instead of a schema mismatch.
                if input.is_null() {
                    permission = Err(MALFORMED_ARGUMENTS.into());
                }
                // Delegate's parser preserves case-insensitive roles and structured domain errors.
                if local
                    && name != "delegate"
                    && let Some(spec) = self.tool_specs.iter().find(|spec| spec.name == name)
                {
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
                if let Some(kind) = self.interrupted() {
                    self.finish_interrupted(kind);
                    return false;
                }
                let ready = match permission {
                    Err(error) => ReadyCall::Rejected(ToolResult::error(error)),
                    Ok(()) => match validation {
                        None => ReadyCall::Local,
                        Some(Err(error)) => ReadyCall::Invalid(error),
                        Some(Ok(call)) => match self.gate_network_tool(&name, &id, &input).await {
                            NetworkGate::Cancelled => return false,
                            NetworkGate::Reject(result) => ReadyCall::Rejected(result),
                            NetworkGate::Proceed => {
                                if self.shared.executor.requires_scope_gate(&name)
                                    || self
                                        .shared
                                        .executor
                                        .tool_permissions(&name)
                                        .is_some_and(|permissions| permissions.network)
                                {
                                    calls.push_back(BatchCall {
                                        id,
                                        name,
                                        input,
                                        ready: ReadyCall::Tool(call.scope_approved()),
                                    });
                                    continue;
                                }
                                let mut cancel = self.channels.cancel_rx.clone();
                                let authorized = tokio::select! {
                                    biased;
                                    _ = cancel.wait_for(|interrupt| interrupt.is_interrupted()) => {
                                        if let Some(kind) = self.interrupted() {
                                            self.finish_interrupted(kind);
                                        }
                                        return false;
                                    }
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
                if let Some(runtime) = self.runtime() {
                    runtime.goal_tool_activity(self.task.run_id, &first.name);
                }
                match self.publish_budget() {
                    crate::budget_tracker::BudgetDecision::Continue => {}
                    crate::budget_tracker::BudgetDecision::Exhausted(_) => return false,
                }
                let mut wave = vec![first];
                if wave[0].awaited_delegate() {
                    self.identical_calls.end_stalled();
                    while calls.front().is_some_and(BatchCall::awaited_delegate) {
                        if let Some(call) = calls.pop_front() {
                            wave.push(call);
                        }
                    }
                    let Some(runtime) = self.runtime() else {
                        for call in wave {
                            self.context.push_tool_result(
                                call.id,
                                ToolResult::error("runtime is unavailable"),
                            );
                            self.publish_message_count();
                        }
                        continue;
                    };
                    let mut ids = Vec::with_capacity(wave.len());
                    let mut children = Vec::with_capacity(wave.len());
                    let mut spawned = Vec::with_capacity(wave.len());
                    for call in wave {
                        if let Err(error) = self.validate_owner_mutation().await {
                            meta::cleanup_delegates(&runtime, spawned).await;
                            self.finish_owner_error("validate", error);
                            return false;
                        }
                        if let Some(kind) = self.interrupted() {
                            if kind == InterruptKind::Cancel {
                                meta::cleanup_delegates(&runtime, spawned).await;
                            }
                            self.finish_interrupted(kind);
                            return false;
                        }
                        let child = match self
                            .guard_team_artifact(&call.name, &call.input)
                            .and_then(|()| {
                                self.policy
                                    .authorize(&call.name)
                                    .map_err(|error| error.to_string())
                            }) {
                            Ok(()) => meta::spawn_delegate(self, &runtime, call.input),
                            Err(error) => Err(meta::DispatchResult {
                                result: ToolResult::error(error),
                                terminal: meta::Terminal::Continue,
                            }),
                        };
                        if let Ok(child) = &child {
                            spawned.push(*child);
                        }
                        ids.push(call.id);
                        children.push(child);
                    }
                    for (id, dispatch) in ids
                        .into_iter()
                        .zip(meta::wait_delegates(self, &runtime, children).await)
                    {
                        self.record_tool_outcome("delegate", &dispatch.result, None);
                        self.context.push_tool_result(id, dispatch.result);
                        self.publish_message_count();
                    }
                    if let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return false;
                    }
                    if let Err(error) = self.validate_owner_mutation().await {
                        self.finish_owner_error("validate", error);
                        return false;
                    }
                    continue;
                }
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
                let search_usage = Arc::new(std::sync::Mutex::new(Vec::<providers::Usage>::new()));
                for (index, call) in wave.into_iter().enumerate() {
                    if let Err(error) = self.validate_owner_mutation().await {
                        self.finish_owner_error("validate", error);
                        return false;
                    }
                    if let Some(kind) = self.interrupted() {
                        self.finish_interrupted(kind);
                        return false;
                    }
                    let BatchCall {
                        id,
                        name,
                        input,
                        ready,
                    } = call;
                    match ready {
                        ReadyCall::Tool(mut call) => {
                            // Every schema/role/global/network gate has passed. Freeze
                            // this call's capability from the chat invocation, never the
                            // preference receiver (which may change while chat streams).
                            if name == "web_search" {
                                let usage = Arc::clone(&search_usage);
                                let sink = Arc::new(move |value| {
                                    usage
                                        .lock()
                                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                                        .push(value);
                                });
                                match invocation_model.web_search_provider(
                                    invocation,
                                    self.task.role,
                                    &self.tool_specs,
                                    sink,
                                ) {
                                    Ok(Some(provider)) => {
                                        call = call.with_search_provider(provider)
                                    }
                                    Ok(None) => {}
                                    Err(error) => {
                                        completed.push((
                                            index,
                                            id,
                                            name,
                                            input,
                                            ReadyCall::Rejected(ToolResult::error(
                                                error.to_string(),
                                            )),
                                        ));
                                        continue;
                                    }
                                }
                            }
                            prepared_wave.push((index, id, name, input, call));
                        }
                        ReadyCall::Invalid(error) => {
                            completed.push((index, id, name, input, ReadyCall::Invalid(error)));
                        }
                        ready => completed.push((index, id, name, input, ready)),
                    }
                }
                // Once the first task starts, every exit must join the wave and
                // drain its completed usage. A mid-spawn elapsed-budget check
                // could otherwise drop a sibling's already received usage.
                match self.publish_budget() {
                    crate::budget_tracker::BudgetDecision::Continue => {}
                    crate::budget_tracker::BudgetDecision::Exhausted(_) => return false,
                }
                for (index, id, name, input, call) in prepared_wave {
                    let metadata = (index, id.clone(), name.clone(), input.clone());
                    let mut cancel = self.channels.cancel_rx.clone();
                    let bus = Arc::clone(&self.shared.bus);
                    let run_id = ctx.run_id.clone();
                    let parent_call_id = ctx.parent_call_id.clone();
                    let handle = tasks.spawn(async move {
                            let ready = tokio::select! {
                                biased;
                                interrupt = cancel.wait_for(|interrupt| interrupt.is_interrupted()) => {
                                    let reason = match interrupt.ok().and_then(|value| value.kind()) {
                                        Some(InterruptKind::Stop) => "stopped",
                                        _ => "cancelled",
                                    };
                                    bus.emit(Event::new(event_bus::ToolEvent::ToolCompleted {
                                        tool_name: name.clone(), call_id: id.clone(), is_error: true,
                                        output: Some(reason.into()), detail: None, run_id: Some(run_id.clone()),
                                        parent_call_id: parent_call_id.clone(),
                                    }));
                                    ReadyCall::Executed(ToolResult::error(reason))
                                },
                                result = call.execute() => match result {
                                    Ok(result) => ReadyCall::Executed(result),
                                    Err(error) => ReadyCall::Failed(error),
                                },
                            };
                            (index, id, name, input, ready)
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
                                        parent_call_id: ctx.parent_call_id.clone(),
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
                // Drain every completed request in the wave before any individual
                // result can exhaust a budget or terminate this run. The sidecar
                // survives tool formatting failures, panic, and cancellation.
                for usage in std::mem::take(
                    &mut *search_usage
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                ) {
                    self.budget.usage(usage);
                    if let Some(runtime) = self.runtime() {
                        runtime.goal_usage(self.task.run_id, self.task.config.purpose, usage);
                    }
                }
                completed.sort_by_key(|(index, ..)| *index);
                // The first call that stalls too often stops the run once the
                // wave's results are recorded.
                let mut stalled = None;
                for (_, id, name, input, ready) in completed {
                    let reports_progress = self.reports_progress(&name, &input);
                    if !reports_progress {
                        self.identical_calls.end_stalled();
                    }
                    if matches!(&ready, ReadyCall::Local)
                        && let Err(error) = self.validate_owner_mutation().await
                    {
                        self.finish_owner_error("validate", error);
                        return false;
                    }
                    // Runtime-owned ledger calls need the same transcript lifecycle,
                    // including calls rejected by preflight before dispatch.
                    self.ledger_tool_started(&name, &id, &input);
                    let observed = matches!(
                        &ready,
                        ReadyCall::Executed(_) | ReadyCall::Failed(_) | ReadyCall::Invalid(_)
                    );
                    let typed_error_class = match &ready {
                        ReadyCall::Failed(error) | ReadyCall::Invalid(error) => {
                            Some(ToolErrorClass::from_tool_error(error))
                        }
                        _ => None,
                    };
                    let result = if let ReadyCall::Rejected(result) | ReadyCall::Executed(result) =
                        ready
                    {
                        result
                    } else if let ReadyCall::Failed(error) = ready {
                        ToolResult::error(error.agent_message())
                    } else if let ReadyCall::Invalid(error) = ready {
                        self.shared.executor.report_invalid_call(
                            &ctx,
                            &name,
                            &id,
                            input.clone(),
                            &error,
                        );
                        ToolResult::error(error.agent_message())
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
                        let progress_input = reports_progress.then(|| input.clone());
                        let dispatch = meta::dispatch(self, &name, input).await;
                        self.ledger_tool_completed(&name, &id, &dispatch.result);
                        let error_class = self.record_tool_outcome(&name, &dispatch.result, None);
                        if let Some(input) = progress_input
                            && self.observe_progress(&name, &input, &dispatch.result)
                            && stalled.is_none()
                        {
                            stalled = Some((id.clone(), name.clone(), input, error_class));
                        }
                        self.context.push_tool_result(id, dispatch.result);
                        self.publish_message_count();
                        match dispatch.terminal {
                            meta::Terminal::Continue => continue,
                            meta::Terminal::Finish(result) => {
                                return self.finish_tool_batch(&result, remaining).await;
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
                    let result = meta::bound_thread_result(&name, result);
                    self.ledger_tool_completed(&name, &id, &result);
                    let error_class = self.record_tool_outcome(&name, &result, typed_error_class);
                    if result.idle_wait_ms > 0 {
                        self.budget
                            .exclude_idle(Duration::from_millis(result.idle_wait_ms));
                    }
                    if reports_progress
                        && self.observe_progress(&name, &input, &result)
                        && stalled.is_none()
                    {
                        stalled = Some((id.clone(), name.clone(), input.clone(), error_class));
                    }
                    let rule_targets_for_call = call_rule_targets(&name, &input);
                    // 停滞検出は観測専用。提案は履歴へ注入せず EscalationProposed
                    // イベントの発行だけを行う (メタ操作分岐は観測対象外)。
                    let observation_paths = match name.as_str() {
                        _ if !observed => Vec::new(),
                        "edit" | "write" => rule_targets_for_call.clone(),
                        // The tool resolves the patch against the executor's workspace.
                        "apply_patch" => patch_rewrite_paths(
                            &input,
                            self.shared.executor.default_cwd().as_deref(),
                        ),
                        _ => Vec::new(),
                    };
                    if observed
                        && let Some(trigger) = self.escalation_detector.observe(
                            &ToolObservation {
                                tool: name.as_str(),
                                paths: observation_paths,
                                is_error: result.is_error,
                            },
                            &self.shared.escalation,
                        )
                    {
                        self.propose_escalation(trigger, &name, &input, &id, error_class);
                    }
                    // Calls a run_js program made count like calls made directly: the
                    // detector sees them (edit loops, error streaks) and so does the budget.
                    let children = if observed && name == tools::tools::run_js::NAME {
                        run_js_host::child_observations(result.detail.as_ref())
                    } else {
                        Vec::new()
                    };
                    for child in &children {
                        let paths = if matches!(child.name.as_str(), "edit" | "write") {
                            child.paths.clone()
                        } else {
                            Vec::new()
                        };
                        if let Some(trigger) = self.escalation_detector.observe(
                            &ToolObservation {
                                tool: child.name.as_str(),
                                paths,
                                is_error: child.is_error,
                            },
                            &self.shared.escalation,
                        ) {
                            self.propose_escalation(trigger, &child.name, &input, &id, None);
                        }
                        if !child.is_error {
                            if child.name == "read"
                                && let Some(path) = child.paths.first()
                            {
                                self.budget.read(path);
                            }
                            self.budget.mark_progress();
                            if matches!(child.name.as_str(), "read" | "edit" | "write") {
                                rule_targets.extend(child.paths.iter().cloned());
                            }
                        }
                    }
                    if observed && !result.is_error {
                        if name == "read"
                            && let Some(path) = rule_targets_for_call.first()
                        {
                            self.budget.read(path);
                        }
                        if !is_meta_op(&name) {
                            self.budget.mark_progress();
                        }
                    }
                    if observed && !result.is_error {
                        rule_targets.extend(rule_targets_for_call);
                    }
                    if observed
                        && !result.is_error
                        && name == "shell"
                        && let Some(cwd) = input.get("cwd").and_then(Value::as_str)
                    {
                        shell_cwds.push(PathBuf::from(cwd));
                    }
                    self.context.push_tool_result(id, result);
                    self.publish_message_count();
                }
                if let Some((id, name, input, error)) = stalled {
                    self.stop_identical_calls(&id, &name, &input, error, true);
                    return false;
                }
                // A completed parallel wave belongs to one assistant tool batch.
                // Preserve all its results before a token budget can stop the run.
                match self.publish_budget() {
                    crate::budget_tracker::BudgetDecision::Continue => {}
                    crate::budget_tracker::BudgetDecision::Exhausted(_) => return false,
                }
                if let Some(kind) = self.interrupted() {
                    self.finish_interrupted(kind);
                    return false;
                }
                if let Err(error) = self.validate_owner_mutation().await {
                    self.finish_owner_error("validate", error);
                    return false;
                }
            }
        }
        let routed = self.route_rule_targets(rule_targets, shell_cwds);
        if !routed.project.is_empty()
            && let Some(session) = &mut self.rules_session
            && let Some(text) = rules::after_successful_tools(session, &routed.project)
        {
            self.context.push_user(&text);
            self.publish_message_count();
        }
        if !routed.allowed.is_empty() {
            self.append_allowed_instructions(routed.allowed);
        }
        true
    }

    fn propose_escalation(
        &mut self,
        trigger: event_bus::EscalationTrigger,
        name: &str,
        input: &Value,
        call_id: &str,
        error_class: Option<ToolErrorClass>,
    ) {
        let run_id = self.caller_run_id().to_string();
        if EscalationDetector::is_no_progress_trigger(&trigger) {
            self.shared.bus.emit(Event::new(DiagnosticEvent {
                source: "escalation_detector".into(),
                severity: DiagnosticSeverity::Warning,
                code: diagnostic_codes::NO_PROGRESS.into(),
                detail: crate::escalation::detector::no_progress_detail(
                    &trigger,
                    name,
                    input,
                    error_class,
                ),
                run_id: Some(run_id.clone()),
                thread_id: None,
                call_id: Some(call_id.to_owned()),
            }));
        }
        self.shared
            .bus
            .emit(Event::new(LifecycleEvent::EscalationProposed {
                run_id,
                trigger,
            }));
    }

    fn shared_call(&self, call: &BatchCall) -> bool {
        shares_wave(&self.shared.executor, &call.name)
    }

    /// Network tools are gated by role and tool policy. Fetch also reviews the actual input.
    async fn gate_network_tool(&mut self, name: &str, call_id: &str, input: &Value) -> NetworkGate {
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
        let decision = if self.shared.executor.requires_scope_gate(name) {
            self.mcp_scope_decision(name)
        } else {
            judge_web_network_access(
                &self.policy.capabilities,
                &self.policy.role_name,
                name,
                per_tool,
                self.runtime()
                    .is_some_and(|runtime| runtime.web_tools_enabled()),
            )
        };
        match decision {
            NetworkAccessDecision::Allow => {
                self.audit_network_call(name, call_id, input, "allow", "tool_policy", "");
                NetworkGate::Proceed
            }
            NetworkAccessDecision::Deny { reason } => {
                if self.shared.executor.requires_scope_gate(name) {
                    self.emit_mcp_scope_denial(name, call_id, &reason);
                }
                self.reject_network_call(name, call_id, input, "tool_policy", &reason)
            }
            NetworkAccessDecision::Ask { mut reason } => {
                if crate::network::reviews_each_call(name) && per_tool == PolicyDecision::AutoAllow
                {
                    let context = self
                        .runtime()
                        .and_then(|runtime| {
                            runtime
                                .shared
                                .review_runs
                                .lock()
                                .ok()
                                .and_then(|runs| runs.get(&self.task.run_id).cloned())
                        })
                        .and_then(|run| {
                            crate::escalation_review::review_context(
                                run,
                                self.shared.rules.as_deref(),
                            )
                            .ok()
                        });
                    let (context, stamp) = match context {
                        Some((context, stamp)) => (Some(context), Some(stamp)),
                        None => (None, None),
                    };
                    let reviewer = crate::escalation_review::QuickModelReviewer::new(Arc::clone(
                        &self.shared.model,
                    ));
                    let review_run_id = self.task.run_id.to_string();
                    let executor = Arc::clone(&self.shared.executor);
                    let review = async {
                        if name != crate::network::UPLOAD_TOOL {
                            return reviewer
                                .review_web_request(&review_run_id, name, input, context)
                                .await;
                        }
                        match executor.browser_upload_review(&review_run_id, input).await {
                            Ok(upload) => {
                                reviewer
                                    .review_upload(&review_run_id, upload, context)
                                    .await
                            }
                            Err(reason) => {
                                Ok(crate::escalation_review::ReviewVerdict::Deny { reason })
                            }
                        }
                    };
                    let verdict = tokio::select! {
                        biased;
                        changed = self.channels.cancel_rx.changed() => {
                            if changed.is_ok() && let Some(kind) = self.interrupted() {
                                self.finish_interrupted(kind);
                                return NetworkGate::Cancelled;
                            }
                            return self.reject_network_call(name, call_id, input, "auto_review", "review was interrupted");
                        }
                        verdict = review => verdict,
                    };
                    match verdict {
                        Ok(crate::escalation_review::ReviewVerdict::Approve) => {
                            let current = self.runtime().and_then(|runtime| {
                                runtime.shared.review_runs.lock().ok().and_then(|runs| {
                                    runs.get(&self.task.run_id).map(|run| run.stamp())
                                })
                            });
                            if !matches!((stamp, current), (Some(expected), Some(current))
                                if current.synced && current == expected)
                            {
                                return self.reject_network_call(
                                    name,
                                    call_id,
                                    input,
                                    "auto_review",
                                    "review context changed during review",
                                );
                            }
                            self.audit_network_call(
                                name,
                                call_id,
                                input,
                                "allow",
                                "auto_review",
                                "approved",
                            );
                            return NetworkGate::Proceed;
                        }
                        // A critical-risk approval is the user's decision: fall
                        // through to the human approval below.
                        Ok(crate::escalation_review::ReviewVerdict::RequiresUser {
                            reason: critical,
                        }) => reason = critical,
                        Ok(crate::escalation_review::ReviewVerdict::Deny { reason: error }) => {
                            return self.reject_network_call(
                                name,
                                call_id,
                                input,
                                "auto_review",
                                &error,
                            );
                        }
                        Err(error) => {
                            let error = error.to_string();
                            return self.reject_network_call(
                                name,
                                call_id,
                                input,
                                "auto_review",
                                &error,
                            );
                        }
                    }
                }
                let gate = ApprovalGate::new(Arc::clone(&self.shared.bus), WEB_APPROVAL_TIMEOUT);
                // Model call IDs are run-local. Scope approval IDs to the run.
                let correlation_id = format!("{}:{}", self.task.run_id, call_id);
                let outcome = tokio::select! {
                    biased;
                    changed = self.channels.cancel_rx.changed() => {
                        if changed.is_ok() && let Some(kind) = self.interrupted() {
                            self.finish_interrupted(kind);
                            return NetworkGate::Cancelled;
                        }
                        return self.reject_network_call(name, call_id, input, "user_review", "approval wait was interrupted");
                    }
                    outcome = gate.request_with_input(name, &correlation_id, Some(input.clone())) => outcome,
                };
                match outcome {
                    ApprovalOutcome::Approved => {
                        self.audit_network_call(
                            name,
                            call_id,
                            input,
                            "allow",
                            "user_review",
                            "approved",
                        );
                        NetworkGate::Proceed
                    }
                    ApprovalOutcome::Denied | ApprovalOutcome::TimedOut => {
                        let error = if matches!(outcome, ApprovalOutcome::TimedOut) {
                            format!("approval timed out: {reason}")
                        } else {
                            format!("approval denied: {reason}")
                        };
                        if self.shared.executor.requires_scope_gate(name) {
                            self.emit_mcp_scope_denial(name, call_id, &reason);
                        }
                        self.reject_network_call(name, call_id, input, "user_review", &error)
                    }
                }
            }
        }
    }

    /// Keep audit/diagnostic reasons bare and add agent guidance only at the
    /// rejection boundary, including policy and stale-context denials.
    fn reject_network_call(
        &self,
        name: &str,
        call_id: &str,
        input: &Value,
        reviewer: &str,
        reason: &str,
    ) -> NetworkGate {
        self.audit_network_call(name, call_id, input, "deny", reviewer, reason);
        NetworkGate::Reject(ToolResult::error(format!(
            "{reason}\n{}",
            crate::escalation_review::DENIAL_GUIDANCE
        )))
    }

    fn audit_network_call(
        &self,
        name: &str,
        call_id: &str,
        input: &Value,
        decision: &str,
        reviewer: &str,
        reason: &str,
    ) {
        let detail = serde_json::json!({
            "tool": name,
            "input": input,
            "decision": decision,
            "reviewer": reviewer,
            "reason": reason,
        })
        .to_string();
        self.shared.bus.emit(Event::new(DiagnosticEvent {
            source: "network_access".into(),
            severity: if decision == "allow" {
                DiagnosticSeverity::Info
            } else {
                DiagnosticSeverity::Warning
            },
            code: diagnostic_codes::TOOL_CALL_ACCESS.into(),
            detail,
            run_id: Some(self.task.run_id.to_string()),
            thread_id: None,
            call_id: Some(call_id.into()),
        }));
    }
}

/// Paths of one call that select project rules once it succeeds: the `path` of
/// read, write, edit and grep, and every path an apply_patch input names, as
/// written and including move sources and deleted files.
fn call_rule_targets(name: &str, input: &Value) -> Vec<PathBuf> {
    match name {
        "read" | "write" | "edit" | "grep" => input
            .get("path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .into_iter()
            .collect(),
        "apply_patch" => input
            .get("input")
            .and_then(Value::as_str)
            .and_then(|patch| apply_patch::targets_raw(patch).ok())
            .map(|targets| targets.into_iter().map(|(raw, _)| raw).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Whether a call to `name` may run in one wave with its neighbors. File
/// writers, apply_patch included, and exclusive tools always run alone.
fn shares_wave(executor: &tools::ToolExecutor, name: &str) -> bool {
    !is_meta_op(name)
        && !matches!(name, "task_claim" | "task_complete" | "finding_append")
        && !executor
            .tool_permissions(name)
            .is_some_and(|permissions| permissions.fs_write)
        && executor.tool_execution_mode(name) == ToolExecutionMode::Shared
}

/// 標準ツール定義を返す。
/// Web tools are filtered by role and the current Web tool switch.
pub(crate) fn standard_tool_specs(executor: &tools::ToolExecutor) -> Vec<ToolSpec> {
    let mut specs: Vec<_> = executor
        .tool_specs()
        .into_iter()
        .map(|spec| ToolSpec {
            name: spec.name,
            description: spec.description,
            input_schema: spec.input_schema,
        })
        .collect();
    specs.extend(META_OPS.iter().map(|name| crate::meta::tool_spec(name)));
    specs
}

/// Drops `inspect_artifact` unless the runtime has pages to open artifacts in,
/// so builds without CEF never offer it.
pub(crate) fn retain_backed_tools(specs: &mut Vec<ToolSpec>, artifact_pages: bool) {
    if !artifact_pages {
        specs.retain(|spec| spec.name != crate::artifacts::INSPECT_ARTIFACT);
    }
}

pub(crate) fn append_subagent_context_note(specs: &mut [ToolSpec], family: crate::ModelFamily) {
    if !matches!(
        family,
        crate::ModelFamily::Gpt5 | crate::ModelFamily::OpenAiReasoning
    ) {
        return;
    }
    for spec in specs {
        if spec.name == "delegate" {
            spec.description.push_str(crate::SUBAGENT_CONTEXT_NOTE);
        }
    }
}

/// モデルに見せるツール定義を決定する。
///
/// role の capability filter を適用した上で、skill レジストリが未接続の
/// ランタイムからは `skill_load` を除く。`skill_load` は capability 上
/// Orchestrator/Worker に許可されているが、レジストリなしでは呼び出しが
/// 必ず失敗するため、失敗前提の定義をモデルに見せない (model only sees
/// tools that can work)。子 run では Direct 専用の escalate も除く。
pub(crate) fn visible_tool_specs(
    specs: Vec<ToolSpec>,
    policy: &ExecutionPolicy,
    skills_configured: bool,
    is_child: bool,
    web_tools_enabled: bool,
) -> Vec<ToolSpec> {
    policy
        .filter_tool_specs(specs)
        .into_iter()
        .filter(|spec| skills_configured || spec.name != "skill_load")
        .filter(|spec| !is_child || spec.name != "escalate")
        .filter(|spec| {
            web_tools_enabled || !matches!(spec.name.as_str(), "web_search" | "web_fetch")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use agents::Role;

    use super::*;
    use crate::ExecutionPolicy;

    fn standard_tool_specs() -> Vec<ToolSpec> {
        let executor = tools::ToolExecutor::with_standard_tools(
            Arc::new(event_bus::EventBus::new(16)),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .with_web_tools()
        .unwrap();
        super::standard_tool_specs(&executor)
    }

    fn names(specs: &[ToolSpec]) -> Vec<&str> {
        specs.iter().map(|s| s.name.as_str()).collect()
    }

    #[test]
    fn standard_tool_specs_preserve_read_and_edit_schemas() {
        // Given: the real standard tool schemas.
        use tools::Tool;
        let expected = [
            ("read", tools::Read.schema()),
            ("edit", tools::Edit.schema()),
            ("write", tools::Write.schema()),
            ("apply_patch", tools::ApplyPatch::new().schema()),
        ];

        // When: runtime builds the model-facing definitions.
        let specs = standard_tool_specs();

        // Then: every parameter schema reaches the model unchanged.
        for (name, schema) in expected {
            let spec = specs.iter().find(|spec| spec.name == name).unwrap();
            assert_eq!(spec.input_schema, schema);
        }
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

    // Given: WebResearcher のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: web_search と web_fetch がモデルに見える
    #[test]
    fn visible_tool_specs_exposes_both_web_tools_for_web_researcher() {
        let policy = ExecutionPolicy::for_role(Role::WebResearcher);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);
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

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);
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

            let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);
            let tool_names = names(&specs);

            assert!(!tool_names.contains(&"web_search"));
            assert!(!tool_names.contains(&"web_fetch"));
        }
    }

    #[test]
    fn visible_tool_specs_exposes_conversation_web_tools_only_when_enabled() {
        let policy = ExecutionPolicy::for_role(Role::Worker).for_run_config(
            &crate::RunConfig {
                conversation: true,
                category: Some("conversation".into()),
                ..Default::default()
            },
            true,
        );
        for enabled in [true, false] {
            let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, enabled);
            let tool_names = names(&specs);
            assert_eq!(tool_names.contains(&"web_search"), enabled);
            assert_eq!(tool_names.contains(&"web_fetch"), enabled);
            for tool in [
                "read",
                "write",
                "edit",
                "apply_patch",
                "shell",
                "grep",
                "git_diff",
            ] {
                assert!(tool_names.contains(&tool), "worker baseline keeps {tool}");
            }
            assert!(!tool_names.contains(&"delegate"));
        }
    }

    // Given: Worker のポリシー (skill_load は capability 内) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load はモデルに見せる定義に残る
    #[test]
    fn visible_tool_specs_keeps_skill_load_for_worker_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true, false, true);

        assert!(names(&specs).contains(&"skill_load"));
    }

    // Given: Worker のポリシー
    // When: visible_tool_specs を呼ぶ
    // Then: escalate がモデルに見える
    #[test]
    fn visible_tool_specs_exposes_escalate_for_worker() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);

        assert!(names(&specs).contains(&"escalate"));
    }

    // A child Worker cannot hand off into another Orchestrator root.
    #[test]
    fn visible_tool_specs_hides_escalate_for_child_worker() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, true, true);

        assert!(!names(&specs).contains(&"escalate"));
        assert!(names(&specs).contains(&"shell"));
    }

    // Given: Worker のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load は除去され、capability 内の通常ツールは保持される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_worker_when_skills_not_configured() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);

        assert!(!names(&specs).contains(&"skill_load"));
        assert!(names(&specs).contains(&"edit"));
        assert!(names(&specs).contains(&"write"));
        assert!(names(&specs).contains(&"apply_patch"));
    }

    // Given: the standard tools and every role's policy
    // When: visible_tool_specs builds each role's definitions
    // Then: apply_patch is shown exactly where edit and write are, to the Worker
    #[test]
    fn visible_tool_specs_expose_apply_patch_only_with_edit_and_write() {
        for role in [
            Role::Orchestrator,
            Role::Explorer,
            Role::Worker,
            Role::Reviewer,
            Role::WebResearcher,
            Role::Planner,
            Role::Advisor,
            Role::MultimodalLooker,
            Role::Evaluator,
        ] {
            let policy = ExecutionPolicy::for_role(role);
            let specs = visible_tool_specs(standard_tool_specs(), &policy, true, false, true);
            let tool_names = names(&specs);
            assert_eq!(
                tool_names.contains(&"apply_patch"),
                tool_names.contains(&"edit") && tool_names.contains(&"write"),
                "{}",
                role.name()
            );
            assert_eq!(
                tool_names.contains(&"apply_patch"),
                role == Role::Worker,
                "{}",
                role.name()
            );
        }
    }

    // Given: calls to file tools, an apply_patch that adds, updates, moves and
    //        deletes, an unparseable patch and a shell command
    // When: their rule targets are collected
    // Then: file tools give their path, the patch gives every path as written,
    //       and the others give none
    #[test]
    fn rule_targets_cover_every_path_a_patch_names() {
        let patch = "\
*** Begin Patch
*** Add File: docs/new.md
+x
*** Update File: src/lib.rs
@@
-a
+b
*** Update File: src/old.rs
*** Move to: src/new.rs
@@
-a
+b
*** Delete File: ./src/gone.rs
*** End Patch";
        assert_eq!(
            call_rule_targets("apply_patch", &serde_json::json!({ "input": patch })),
            [
                "docs/new.md",
                "src/lib.rs",
                "src/old.rs",
                "src/new.rs",
                "./src/gone.rs",
            ]
            .map(PathBuf::from)
        );
        for name in ["read", "write", "edit", "grep"] {
            assert_eq!(
                call_rule_targets(name, &serde_json::json!({ "path": "src/lib.rs" })),
                [PathBuf::from("src/lib.rs")],
                "{name}"
            );
        }
        for (name, input) in [
            ("apply_patch", serde_json::json!({ "input": "garbage" })),
            ("apply_patch", serde_json::json!({ "path": "src/lib.rs" })),
            ("shell", serde_json::json!({ "command": "cat src/lib.rs" })),
            ("read", serde_json::json!({})),
        ] {
            assert!(call_rule_targets(name, &input).is_empty(), "{name} {input}");
        }
    }

    // Given: the standard tools
    // When: deciding which calls may share a parallel wave
    // Then: read-only shared tools may, while apply_patch, like every file
    //       writer, runs alone
    #[test]
    fn apply_patch_never_shares_a_wave() {
        let executor = tools::ToolExecutor::with_standard_tools(
            Arc::new(event_bus::EventBus::new(16)),
            Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        for (name, shared) in [
            ("read", true),
            ("grep", true),
            ("apply_patch", false),
            ("edit", false),
            ("write", false),
            ("shell", false),
            ("finish", false),
        ] {
            assert_eq!(shares_wave(&executor, name), shared, "{name}");
        }
    }

    // Given: Explorer のポリシー (skill_load は capability 外) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: capability filter により skill_load は除去される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_explorer_even_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Explorer);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true, false, true);

        assert!(!names(&specs).contains(&"skill_load"));
    }

    // Given: Orchestrator のポリシー (skill_load は capability 内) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load はモデルに見せる定義に残る
    #[test]
    fn visible_tool_specs_keeps_skill_load_for_orchestrator_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true, false, true);

        assert!(names(&specs).contains(&"skill_load"));
    }

    #[test]
    fn visible_tool_specs_hides_web_tools_when_disabled() {
        let policy = ExecutionPolicy::for_role(Role::WebResearcher);
        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, false);
        let tool_names = names(&specs);
        assert!(!tool_names.contains(&"web_search"));
        assert!(!tool_names.contains(&"web_fetch"));
    }

    // Given: Orchestrator のポリシーと skills 未設定
    // When: visible_tool_specs を呼ぶ
    // Then: capability 内でも skill_load は除去される
    #[test]
    fn visible_tool_specs_drops_skill_load_for_orchestrator_when_skills_not_configured() {
        let policy = ExecutionPolicy::for_role(Role::Orchestrator);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, false, false, true);

        assert!(!names(&specs).contains(&"skill_load"));
    }

    // Given: GPT-family モデル
    // When: delegate 系のツール定義を組み立てる
    // Then: subagent context note が含まれる
    #[test]
    fn delegate_specs_include_context_note_for_gpt_family() {
        for family in [
            crate::ModelFamily::Gpt5,
            crate::ModelFamily::OpenAiReasoning,
        ] {
            let mut specs = standard_tool_specs();
            append_subagent_context_note(&mut specs, family);

            let spec = specs.iter().find(|spec| spec.name == "delegate").unwrap();
            assert!(spec.description.contains(crate::SUBAGENT_CONTEXT_NOTE));
        }
    }

    // Given: Claude と Kimi モデル
    // When: delegate 系のツール定義を組み立てる
    // Then: subagent context note は含まれない
    #[test]
    fn delegate_specs_exclude_context_note_for_non_gpt_families() {
        for family in [crate::ModelFamily::Claude, crate::ModelFamily::Kimi] {
            let mut specs = standard_tool_specs();
            append_subagent_context_note(&mut specs, family);

            let spec = specs.iter().find(|spec| spec.name == "delegate").unwrap();
            assert!(!spec.description.contains(crate::SUBAGENT_CONTEXT_NOTE));
        }
    }
}
