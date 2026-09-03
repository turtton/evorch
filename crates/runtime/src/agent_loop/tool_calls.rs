use providers::ToolSpec;
use tools::{ToolExecutionContext, ToolResult};

use super::LoopState;
use crate::{ExecutionPolicy, META_OPS, is_meta_op, meta};

impl LoopState {
    pub(super) async fn execute_tools(
        &mut self,
        tool_uses: Vec<(String, String, serde_json::Value)>,
    ) -> bool {
        let ctx = ToolExecutionContext {
            run_id: self.task.run_id.to_string(),
        };
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
                match execution {
                    Ok(result) => result,
                    Err(error) => ToolResult::error(error.to_string()),
                }
            };
            self.context.push_tool_result(id, result);
            self.publish_message_count();
        }
        true
    }
}

pub(super) fn standard_tool_specs() -> Vec<ToolSpec> {
    ["read", "edit", "grep", "shell", "git_diff"]
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

    // Given: Worker のポリシー (skill_load は capability 内) と skills 設定あり
    // When: visible_tool_specs を呼ぶ
    // Then: skill_load はモデルに見せる定義に残る
    #[test]
    fn visible_tool_specs_keeps_skill_load_for_worker_when_skills_configured() {
        let policy = ExecutionPolicy::for_role(Role::Worker);

        let specs = visible_tool_specs(standard_tool_specs(), &policy, true);

        assert!(names(&specs).contains(&"skill_load"));
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
