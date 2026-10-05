//! ファイルの新規作成と全文置換。部分置換には edit を使う。

use std::sync::Arc;

use crate::post_edit::{
    PostEditHook, PostEditInput, UnavailableReason, apply_post_edit, apply_unavailable, input_fits,
};

use std::path::Path;

use super::edit::{required_str, write_atomically};
use super::file_diff;
use crate::{Permissions, Tool, ToolError, ToolExecutionMode, ToolResult};

/// UTF-8 ファイルを原子的に作成・上書きするツール。
#[derive(Clone, Default)]
pub struct Write {
    post_edit: Option<Arc<dyn PostEditHook>>,
}

// Preserve the existing hook-free value constructor as well as the type name.
#[allow(non_upper_case_globals)]
pub const Write: Write = Write { post_edit: None };

impl std::fmt::Debug for Write {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Write")
            .field("post_edit", &self.post_edit.is_some())
            .finish()
    }
}

impl Write {
    pub fn with_post_edit_hook(mut self, hook: Arc<dyn PostEditHook>) -> Self {
        self.post_edit = Some(hook);
        self
    }
}

#[async_trait::async_trait]
impl Tool for Write {
    fn name(&self) -> &'static str {
        "write"
    }

    fn description(&self) -> &str {
        "Create a UTF-8 file or replace its entire contents atomically using path and content. The parent directory must already exist. Use edit for targeted replacements in an existing file."
    }

    fn schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string" },
                "content": { "type": "string" }
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }

    /// 編集の承認分類は不変。追加検査のプロセス起動は executor が別途承認する。
    fn permissions(&self) -> Permissions {
        Permissions::read_write()
    }

    fn execution_mode(&self) -> ToolExecutionMode {
        ToolExecutionMode::Exclusive
    }

    async fn execute(&self, args: serde_json::Value) -> Result<ToolResult, ToolError> {
        let path = required_str(&args, "path")?;
        let content = required_str(&args, "content")?;
        let before = file_diff::read_previous(Path::new(path));
        write_atomically(Path::new(path), content)?;
        let output = match &before {
            Ok(previous) => file_diff::changed_file(path, previous.as_deref(), content),
            Err(reason) => format!("Wrote {path}; diff unavailable: {reason}"),
        };
        let mut result = ToolResult::success(output);
        if let Some(hook) = &self.post_edit {
            let previous = match before {
                Ok(previous) => previous,
                Err(reason) => {
                    apply_unavailable(
                        hook.as_ref(),
                        UnavailableReason::PreviousContentsUnavailable,
                        reason,
                        &mut result,
                    );
                    return Ok(result);
                }
            };
            if previous.as_deref() == Some(content) {
                return Ok(result);
            }
            if !input_fits(
                Path::new(path),
                &[previous.as_deref().unwrap_or_default(), content],
            ) {
                apply_unavailable(
                    hook.as_ref(),
                    UnavailableReason::InputTooLarge,
                    "checker input exceeds 2 MiB",
                    &mut result,
                );
                return Ok(result);
            }
            // Existing files use upstream Edit to protect unchanged old comments.
            let input = match previous {
                Some(previous) => PostEditInput {
                    tool_name: "Edit",
                    file_path: path.into(),
                    content: None,
                    old_string: Some(previous),
                    new_string: Some(content.to_owned()),
                },
                None => PostEditInput {
                    tool_name: "Write",
                    file_path: path.into(),
                    content: Some(content.to_owned()),
                    old_string: None,
                    new_string: None,
                },
            };
            apply_post_edit(hook.as_ref(), &input, &mut result).await;
        }
        Ok(result)
    }
}
