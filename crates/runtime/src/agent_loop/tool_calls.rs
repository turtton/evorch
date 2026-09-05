use std::sync::Arc;
use std::time::Duration;

use providers::ToolSpec;
use sandbox::{ApprovalGate, ApprovalOutcome, PolicyDecision};
use serde_json::Value;
use tools::{ToolExecutionContext, ToolResult};

use super::LoopState;
use crate::network::{NetworkAccessDecision, judge_web_network_access};
use crate::{ExecutionPolicy, META_OPS, is_meta_op, meta, rules};

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

impl LoopState {
    pub(super) async fn execute_tools(
        &mut self,
        tool_uses: Vec<(String, String, serde_json::Value)>,
    ) -> bool {
        let ctx = ToolExecutionContext {
            run_id: self.task.run_id.to_string(),
        };
        let mut rule_targets = Vec::new();
        for (id, name, input) in tool_uses {
            if self.cancelled() {
                self.finish_cancelled();
                return false;
            }
            let result = if let Err(error) = self.policy.authorize(&name) {
                ToolResult::error(error.to_string())
            } else if is_meta_op(&name) {
                let dispatch = meta::dispatch(self, &name, input).await;
                self.context.push_tool_result(id, dispatch.result);
                self.publish_message_count();
                if let Some(result) = dispatch.finish {
                    self.push_final_result(&result);
                    self.finish_success();
                    return false;
                }
                continue;
            } else {
                match self.gate_network_tool(&name, &id).await {
                    NetworkGate::Proceed => {}
                    NetworkGate::Reject(result) => {
                        self.context.push_tool_result(id, result);
                        self.publish_message_count();
                        continue;
                    }
                    NetworkGate::Cancelled => return false,
                }
                let rule_target = matches!(name.as_str(), "read" | "edit" | "grep")
                    .then(|| input.get("path").and_then(Value::as_str).map(Into::into))
                    .flatten();
                let execution = tokio::select! {
                    biased;
                    changed = self.channels.cancel_rx.changed() => {
                        if changed.is_ok() && self.cancelled() {
                            self.finish_cancelled();
                            return false;
                        }
                        continue;
                    }
                    result = self.shared.executor.execute(&ctx, &name, &id, input) => result,
                };
                let result = match execution {
                    Ok(result) => result,
                    Err(error) => ToolResult::error(error.to_string()),
                };
                if !result.is_error
                    && let Some(target) = rule_target
                {
                    rule_targets.push(target);
                }
                result
            };
            self.context.push_tool_result(id, result);
            self.publish_message_count();
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
