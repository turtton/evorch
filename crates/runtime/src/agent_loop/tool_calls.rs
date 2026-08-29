use providers::ToolSpec;
use tools::ToolResult;

use super::LoopState;
use crate::is_meta_op;

const META_OP_PLACEHOLDER: &str = "メタ操作は v0.2 ディスパッチャで提供予定";

impl LoopState {
    pub(super) async fn execute_tools(
        &mut self,
        tool_uses: Vec<(String, String, serde_json::Value)>,
    ) -> bool {
        for (id, name, input) in tool_uses {
            if self.cancelled() {
                self.finish_cancelled();
                return false;
            }
            let result = if is_meta_op(&name) {
                ToolResult::error(META_OP_PLACEHOLDER)
            } else if let Err(error) = self.policy.authorize(&name) {
                ToolResult::error(error.to_string())
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
                    result = self.shared.executor.execute(&name, &id, input) => result,
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
        .map(|name| ToolSpec {
            name: name.to_string(),
            description: format!("{name} tool"),
            input_schema: serde_json::json!({ "type": "object" }),
        })
        .collect()
}
