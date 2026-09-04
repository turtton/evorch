//! goal 投入を runtime の background run 起動へ接続する production CommandSink (issue #71)。

// allow: SIZE_OK - RuntimeCommandSink 本体に、pinned された 6 件の振る舞いテスト
// (stub モデル込み) が inline テスト慣習どおり同居するため分割不可能。
// テストを別ファイルへ分離すると impl+test ペアリング規約に反する。

use runtime::{AgentRuntime, RunConfig};

use crate::model::commands::{
    CommandSink, GoalSubmission, LoopEvent, ReferenceKind, WorkbenchCommand,
};

/// goal 投入を runtime の background run 起動へ接続する production CommandSink。
///
/// SubmitGoal ごとに goal-N を採番し、entry pre-routing (EntryRouter) で判定した
/// role (Direct→Worker / Coordinated→Orchestrator) の background run を起動する (issue #71)。
pub struct RuntimeCommandSink {
    runtime: AgentRuntime,
    handle: tokio::runtime::Handle,
    accepted_goals: u64,
}

impl RuntimeCommandSink {
    /// runtime と tokio ハンドルから sink を生成する。
    pub fn new(runtime: AgentRuntime, handle: tokio::runtime::Handle) -> Self {
        Self {
            runtime,
            handle,
            accepted_goals: 0,
        }
    }
}

impl CommandSink for RuntimeCommandSink {
    fn submit(&mut self, command: WorkbenchCommand) -> Vec<LoopEvent> {
        match command {
            WorkbenchCommand::SubmitGoal(submission) => {
                self.accepted_goals = self.accepted_goals.saturating_add(1);
                let goal_id = format!("goal-{}", self.accepted_goals);
                let prompt = render_entry_prompt(&submission);
                let runtime = self.runtime.clone();
                let goal_for_log = submission.goal.clone();
                let thread_id = submission.thread_id.clone();
                let goal_id_for_run = goal_id.clone();
                self.handle.spawn(async move {
                    let decision = runtime.entry_router().classify(&goal_for_log).await;
                    runtime.delegate_background(
                        decision.role(),
                        prompt,
                        RunConfig {
                            name: Some(goal_id_for_run),
                            ..RunConfig::default()
                        },
                    );
                });
                vec![LoopEvent::GoalAccepted { thread_id, goal_id }]
            }
            WorkbenchCommand::DecideMerge(_) => {
                tracing::warn!("merge decision has no production loop yet");
                Vec::new()
            }
        }
    }
}

/// GoalSubmission を background run へ渡す entry prompt として整形する。
///
/// goal 本文を先頭に置き、references / constraints は空でない場合のみ
/// `References:` / `Constraints:` セクションとして 1 行 1 項目で続ける。
/// 分類 (`EntryRouter::classify`) は goal 本文のみを受け、references /
/// constraints は起動される run の prompt 側にのみ載る。
pub fn render_entry_prompt(submission: &GoalSubmission) -> String {
    let mut prompt = submission.goal.clone();
    if !submission.references.is_empty() {
        prompt.push_str("\n\nReferences:\n");
        let lines: Vec<String> = submission
            .references
            .iter()
            .map(|reference| {
                let kind_label = match reference.kind {
                    ReferenceKind::Packet => "packet",
                    ReferenceKind::Issue => "issue",
                };
                format!("- {kind_label}: {}", reference.value)
            })
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    if !submission.constraints.is_empty() {
        prompt.push_str("\n\nConstraints:\n");
        let lines: Vec<String> = submission
            .constraints
            .iter()
            .map(|constraint| format!("- {constraint}"))
            .collect();
        prompt.push_str(&lines.join("\n"));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use async_trait::async_trait;
    use event_bus::EventBus;
    use providers::{
        ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
    };
    use runtime::{
        AgentInvocationContext, AgentModel, AgentRuntime, AgentSummary, Role, RuntimeError,
    };
    use tools::ToolExecutor;

    use super::{RuntimeCommandSink, render_entry_prompt};
    use crate::model::commands::{
        CommandSink, GoalSubmission, LoopEvent, MergeCommand, MergeDecision, PacketReference,
        ReferenceKind, WorkbenchCommand,
    };

    /// どんなプロンプトにも即答でテキスト応答 (Stop) を返し、起動された run を
    /// 確実に終端させるテスト用 stub モデル。
    struct AlwaysStopModel;

    #[async_trait]
    impl AgentModel for AlwaysStopModel {
        async fn complete(
            &self,
            _invocation: &AgentInvocationContext,
            _role: Role,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<ChatResponse, RuntimeError> {
            Ok(ChatResponse {
                message: Message {
                    role: MessageRole::Assistant,
                    content: vec![ContentBlock::Text {
                        text: "run finished".to_string(),
                    }],
                },
                usage: Usage::default(),
                finish_reason: FinishReason::Stop,
            })
        }

        fn selected_model(&self, role: Role) -> String {
            format!("test-{}", role.name().to_lowercase())
        }
    }

    fn submission(
        goal: &str,
        references: Vec<PacketReference>,
        constraints: Vec<String>,
    ) -> GoalSubmission {
        GoalSubmission {
            project_id: "evorch".into(),
            thread_id: "thread-1".into(),
            goal: goal.into(),
            references,
            constraints,
        }
    }

    /// マルチスレッド tokio runtime 上に実 AgentRuntime を接続した sink を組み立てる。
    fn build_sink() -> (tokio::runtime::Runtime, RuntimeCommandSink, AgentRuntime) {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let bus = Arc::new(EventBus::new(64));
        let executor = Arc::new(ToolExecutor::new(bus.clone()));
        let model = Arc::new(AlwaysStopModel);
        let runtime = AgentRuntime::new(bus, executor, model);
        let sink = RuntimeCommandSink::new(runtime.clone(), rt.handle().clone());
        (rt, sink, runtime)
    }

    /// predicate を満たす agent 行が現れるまで 50ms 間隔で最大 5 秒待つ。
    fn wait_for_agents(
        runtime: &AgentRuntime,
        predicate: impl Fn(&AgentSummary) -> bool,
    ) -> Vec<AgentSummary> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let agents = runtime.list_agents();
            if agents.iter().any(&predicate) {
                return agents;
            }
            assert!(
                Instant::now() < deadline,
                "agent row did not appear within 5s: {:?}",
                runtime.list_agents()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // Given: references も constraints も空の GoalSubmission
    // When: render_entry_prompt する
    // Then: goal 本文そのものが返る
    #[test]
    fn render_entry_prompt_is_goal_only_when_no_references_or_constraints() {
        let input = submission("fix the typo in README", Vec::new(), Vec::new());

        let prompt = render_entry_prompt(&input);

        assert_eq!(prompt, "fix the typo in README");
    }

    // Given: references 2 件・constraints 1 件の GoalSubmission
    // When: render_entry_prompt する
    // Then: goal の後に References: / Constraints: セクションが順に付く
    #[test]
    fn render_entry_prompt_appends_reference_and_constraint_sections() {
        let input = submission(
            "implement issue #65",
            vec![
                PacketReference {
                    kind: ReferenceKind::Packet,
                    value: "v02-entry-pre-routing".into(),
                },
                PacketReference {
                    kind: ReferenceKind::Issue,
                    value: "65".into(),
                },
            ],
            vec!["model only".into()],
        );

        let prompt = render_entry_prompt(&input);

        assert_eq!(
            prompt,
            "implement issue #65\n\nReferences:\n- packet: v02-entry-pre-routing\n- issue: 65\n\nConstraints:\n- model only"
        );
    }

    // Given: 実 runtime を接続した sink
    // When: SubmitGoal を 2 回 submit する
    // Then: goal-1 / goal-2 の GoalAccepted がそれぞれちょうど 1 件ずつ返る
    #[test]
    fn submit_goal_returns_goal_accepted_with_sequential_ids_and_nothing_else() {
        let (_rt, mut sink, _runtime) = build_sink();

        let first = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement issue #65",
            Vec::new(),
            Vec::new(),
        )));
        let second = sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: fix the typo in README",
            Vec::new(),
            Vec::new(),
        )));

        assert_eq!(
            first,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-1".into(),
            }]
        );
        assert_eq!(
            second,
            vec![LoopEvent::GoalAccepted {
                thread_id: "thread-1".into(),
                goal_id: "goal-2".into(),
            }]
        );
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードつき goal を submit する
    // Then: role Worker・名前 goal-1 の run が現れ、Orchestrator run は現れない
    #[test]
    fn direct_goal_starts_a_worker_run_named_after_the_goal_id() {
        let (_rt, mut sink, runtime) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "direct: fix the typo in README",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&runtime, |agent| {
            agent.role_name == "Worker" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Orchestrator"));
    }

    // Given: 実 runtime を接続した sink
    // When: direct キーワードを含まない goal を submit する
    // Then: role Orchestrator・名前 goal-1 の run が現れ、Worker run は現れない
    #[test]
    fn plain_goal_starts_an_orchestrator_run() {
        let (_rt, mut sink, runtime) = build_sink();

        sink.submit(WorkbenchCommand::SubmitGoal(submission(
            "implement issue #65",
            Vec::new(),
            Vec::new(),
        )));

        let agents = wait_for_agents(&runtime, |agent| {
            agent.role_name == "Orchestrator" && agent.name == "goal-1"
        });
        assert!(!agents.iter().any(|agent| agent.role_name == "Worker"));
    }

    // Given: 実 runtime を接続した sink
    // When: DecideMerge を submit する
    // Then: LoopEvent は空で、run も 1 つも起動されない
    #[test]
    fn decide_merge_emits_no_loop_events_and_starts_no_run() {
        let (_rt, mut sink, runtime) = build_sink();

        let events = sink.submit(WorkbenchCommand::DecideMerge(MergeCommand {
            thread_id: "thread-1".into(),
            pr: None,
            decision: MergeDecision::Approve,
        }));

        assert!(events.is_empty());
        std::thread::sleep(Duration::from_millis(200));
        assert!(runtime.list_agents().is_empty());
    }
}
