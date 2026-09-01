//! ツール実行の窓口（ToolExecutor）。
//!
//! 登録されたツールへの引数スキーマ検証、イベントバスへの開始・完了イベント
//! 発行、結果本文の制御マーカエスケープ（ADR 0008）を一貫して担う。ツールの
//! 実行は必ずこの Executor 経由で行うこと。

use std::collections::HashMap;
use std::sync::Arc;

use event_bus::{Event, EventBus, ToolEvent};
use sandbox::{
    Action, ApprovalGate, ApprovalOutcome, ApprovalPolicy, BwrapConfig, Capabilities, Sandbox,
    SandboxError, resolve,
};

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
    /// ツール能力を実行操作へ分類する方針。
    policy: ApprovalPolicy,
    /// 利用者の承認応答を待つ任意のゲート。
    gate: Option<ApprovalGate>,
}

impl ToolExecutor {
    /// イベントバスを紐付いた空の実行器を生成する。
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            event_bus,
            tools: HashMap::new(),
            policy: ApprovalPolicy::allow_all(),
            gate: None,
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
    /// 低レベルなサンドボックス注入 API である。渡された `Arc<dyn Sandbox>` は
    /// 検証・変換されずそのまま使われるため、隔離なしの `DirectSandbox` も
    /// 注入できる。テスト・記録用サンドボックス・独自統合向けであり、
    /// production の呼び出し元は必ず `with_production_sandbox` を使うこと。
    ///
    /// # Panics
    ///
    /// 標準ツールのスキーマがコンパイルできない場合のみ panic する。標準ツールの
    /// スキーマは `tools::tools::tests::all_standard_tool_schemas_compile` で
    /// コンパイル可能を検証済みのため、到達しない経路である。
    pub fn with_standard_tools(event_bus: Arc<EventBus>, sandbox: Arc<dyn Sandbox>) -> Self {
        let mut executor = Self::new(event_bus);
        let standard: [Arc<dyn Tool>; 5] = [
            Arc::new(Read),
            Arc::new(Edit),
            Arc::new(Grep),
            Arc::new(Shell::new(Arc::clone(&sandbox))),
            Arc::new(GitDiff::new(sandbox)),
        ];
        for tool in standard {
            executor
                .register(tool)
                // SAFE-EXPECT: 標準スキーマは全件をクレート内テストでコンパイル検証している。
                .expect("標準ツールのスキーマは all_standard_tool_schemas_compile でコンパイル可能を検証済み");
        }
        executor
    }

    /// read / edit / grep / shell / git_diff の 5 標準ツールを登録し、production 用の
    /// fail-closed なサンドボックスを注入した実行器を生成する。
    ///
    /// `sandbox::production_sandbox`（composition root）経由で `BwrapSandbox` を
    /// 構築する。bwrap の検出に失敗した場合はフォールバックせずエラーを返す。
    ///
    /// # Errors
    ///
    /// bwrap を検出・機能確認できない場合は `SandboxError` を返す。
    pub fn with_production_sandbox(
        event_bus: Arc<EventBus>,
        config: BwrapConfig,
    ) -> Result<Self, SandboxError> {
        Ok(Self::with_standard_tools(
            event_bus,
            sandbox::production_sandbox(config)?,
        ))
    }

    /// 実行判定に使う承認方針を設定する。
    pub fn set_policy(&mut self, policy: ApprovalPolicy) -> &mut Self {
        self.policy = policy;
        self
    }

    /// 利用者承認を待つゲートを設定する。
    pub fn set_approval_gate(&mut self, gate: ApprovalGate) -> &mut Self {
        self.gate = Some(gate);
        self
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
            self.emit_completed(tool_name, call_id, true, None);
            return Err(error);
        }

        let permissions = registered.tool.permissions();
        let capabilities = Capabilities {
            fs_read: permissions.fs_read,
            fs_write: permissions.fs_write,
            process_spawn: permissions.process_spawn,
            network: permissions.network,
        };
        let action = resolve(
            self.policy.classify(tool_name, &capabilities),
            self.policy.mode(),
        );
        let outcome = match action {
            Action::Proceed => registered.tool.execute(args).await,
            Action::Deny => {
                return self.deny(tool_name, call_id, "policy により拒否されました");
            }
            Action::AskFirst => {
                let Some(gate) = &self.gate else {
                    return self.deny(tool_name, call_id, "承認ゲートが未設定のため拒否されました");
                };
                match gate.request(tool_name, call_id).await {
                    ApprovalOutcome::Approved => registered.tool.execute(args).await,
                    ApprovalOutcome::Denied => {
                        return self.deny(tool_name, call_id, "承認要求が拒否されました");
                    }
                    ApprovalOutcome::TimedOut => {
                        return self.deny(tool_name, call_id, "承認応答がタイムアウトしました");
                    }
                }
            }
            Action::AskOnFailure => {
                let first = registered.tool.execute(args.clone()).await;
                if !is_failure(&first) {
                    first
                } else if let Some(gate) = &self.gate {
                    match gate.request(tool_name, call_id).await {
                        ApprovalOutcome::Approved => registered.tool.execute(args).await,
                        ApprovalOutcome::Denied | ApprovalOutcome::TimedOut => first,
                    }
                } else {
                    first
                }
            }
        };
        match outcome {
            Ok(result) => {
                let content = escape_control_markers(&result.content);
                self.emit_completed(tool_name, call_id, result.is_error, result.detail.clone());
                Ok(ToolResult {
                    content,
                    is_error: result.is_error,
                    detail: result.detail,
                })
            }
            Err(error) => {
                self.emit_completed(tool_name, call_id, true, None);
                Err(error)
            }
        }
    }

    /// ToolCompleted イベントを発行する。
    fn emit_completed(
        &self,
        tool_name: &str,
        call_id: &str,
        is_error: bool,
        detail: Option<serde_json::Value>,
    ) {
        self.event_bus.emit(Event::new(ToolEvent::ToolCompleted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            is_error,
            detail,
        }));
    }

    fn deny<T>(&self, tool_name: &str, call_id: &str, reason: &str) -> Result<T, ToolError> {
        self.event_bus.emit(Event::new(ToolEvent::ExecutionDenied {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        }));
        self.emit_completed(tool_name, call_id, true, None);
        Err(ToolError::ExecutionDenied {
            tool_name: tool_name.to_string(),
            reason: reason.to_string(),
        })
    }
}

fn is_failure(outcome: &Result<ToolResult, ToolError>) -> bool {
    match outcome {
        Ok(result) => result.is_error,
        Err(_) => true,
    }
}
