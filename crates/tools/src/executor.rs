//! ツール実行の窓口（ToolExecutor）。
//!
//! 登録されたツールへの引数スキーマ検証、イベントバスへの開始・完了イベント
//! 発行、結果本文の制御マーカエスケープ（ADR 0008）を一貫して担う。ツールの
//! 実行は必ずこの Executor 経由で行うこと。

use std::collections::HashMap;
use std::sync::Arc;

use event_bus::{Event, EventBus, ToolEvent};

use crate::error::ToolError;
use crate::result::ToolResult;
use crate::sanitize::escape_control_markers;
use crate::schema;
use crate::tool::Tool;
use crate::tools::{Edit, GitDiff, Grep, Read, Shell};

/// ツールとコンパイル済みスキーマ検証器の登録エントリ。
struct RegisteredTool {
    /// 登録されたツール。
    tool: Arc<dyn Tool>,
    /// [`Tool::schema`] をコンパイルした検証器。
    validator: jsonschema::Validator,
}

/// ツール実行の窓口。
///
/// [`ToolExecutor::execute`] は次の順で処理する:
/// 1. 未登録のツール名なら [`ToolError::UnknownTool`]（イベント発行なし）
/// 2. `ToolStarted` をイベントバスへ発行
/// 3. 引数を検証し、違反なら `ToolCompleted(is_error=true)` を発行して
///    [`ToolError::InvalidArgs`] を返す
/// 4. ツールを実行し、失敗なら `ToolCompleted(is_error=true)` を発行して
///    エラーを伝播する
/// 5. 成功なら本文の制御マーカをエスケープし、`ToolCompleted` を発行して
///    結果を返す
pub struct ToolExecutor {
    /// イベントの発行先。
    event_bus: Arc<EventBus>,
    /// ツール名から登録エントリへの対応。
    tools: HashMap<&'static str, RegisteredTool>,
}

impl ToolExecutor {
    /// イベントバスを紐付いた空の実行器を生成する。
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            event_bus,
            tools: HashMap::new(),
        }
    }

    /// ツールを登録し、スキーマを検証器としてキャッシュする。
    ///
    /// 同名ツールの再登録ではエントリごと差し替えるため、旧検証器が残る
    /// ことはない。
    ///
    /// # Errors
    ///
    /// スキーマのコンパイルに失敗した場合は [`ToolError::InvalidSchema`] を返す。
    pub fn register(&mut self, tool: Arc<dyn Tool>) -> Result<(), ToolError> {
        let validator = schema::compile(tool.name(), &tool.schema())?;
        self.tools
            .insert(tool.name(), RegisteredTool { tool, validator });
        Ok(())
    }

    /// read / edit / grep / shell / git_diff の 5 標準ツールを登録した実行器を
    /// 生成する。
    ///
    /// # Panics
    ///
    /// 標準ツールのスキーマがコンパイルできない場合のみ panic する。標準ツールの
    /// スキーマは `tools::tools::tests::all_standard_tool_schemas_compile` で
    /// コンパイル可能を検証済みのため、到達しない経路である。
    pub fn with_standard_tools(event_bus: Arc<EventBus>) -> Self {
        let mut executor = Self::new(event_bus);
        let standard: [Arc<dyn Tool>; 5] = [
            Arc::new(Read),
            Arc::new(Edit),
            Arc::new(Grep),
            Arc::new(Shell),
            Arc::new(GitDiff),
        ];
        for tool in standard {
            executor
                .register(tool)
                .expect("標準ツールのスキーマは all_standard_tool_schemas_compile でコンパイル可能を検証済み");
        }
        executor
    }

    /// ツールを実行する。
    ///
    /// # Errors
    ///
    /// 未登録のツール名なら [`ToolError::UnknownTool`]、引数がスキーマに適合
    /// しなければ [`ToolError::InvalidArgs`] を返す。ツール本体の実行が失敗した
    /// 場合はそのエラーをそのまま伝播する。
    pub async fn execute(
        &self,
        tool_name: &str,
        call_id: &str,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let Some(registered) = self.tools.get(tool_name) else {
            return Err(ToolError::UnknownTool {
                name: tool_name.to_string(),
            });
        };

        self.event_bus.emit(Event::new(ToolEvent::ToolStarted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
        }));

        if let Err(error) = schema::validate_args(&registered.validator, &args) {
            self.emit_completed(tool_name, call_id, true);
            return Err(error);
        }

        let tool = Arc::clone(&registered.tool);
        match tool.execute(args).await {
            Ok(result) => {
                let content = escape_control_markers(&result.content);
                self.emit_completed(tool_name, call_id, result.is_error);
                Ok(ToolResult {
                    content,
                    is_error: result.is_error,
                })
            }
            Err(error) => {
                self.emit_completed(tool_name, call_id, true);
                Err(error)
            }
        }
    }

    /// ToolCompleted イベントを発行する。
    fn emit_completed(&self, tool_name: &str, call_id: &str, is_error: bool) {
        self.event_bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            is_error,
        }));
    }
}
