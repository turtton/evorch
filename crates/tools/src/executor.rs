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
use crate::network_guard::NetworkGuardError;
use crate::origin::derive_content_origin;
use crate::result::ToolResult;
use crate::sanitize::{escape_control_markers, escape_control_markers_in_value};
use crate::schema;
use crate::tool::Tool;
use crate::tools::{Edit, GitDiff, Grep, Read, Shell, WebFetch, WebSearch};

/// ツール実行時の文脈情報。
///
/// [`ToolExecutor::execute`] の必須引数であり、呼び出し元 (AgentRun) が
/// その実行を一意に識別する `run_id` を運ぶ。Executor は `ToolStarted` /
/// `ToolCompleted` イベントへこの値を stamp し、イベントと run の相関を
/// 可能にする。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolExecutionContext {
    /// 実行元 AgentRun の識別子 (例: `run-7`)。
    pub run_id: String,
}

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
/// 5. 成功なら権限宣言から出力の由来を機械導出し (AC5)、本文と detail 内の
///    文字列値から制御マーカをエスケープして `ToolCompleted` を発行し、
///    正規化済みの結果を返す
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
    /// web_search / web_fetch が必要な呼び出し元は返り値に
    /// [`ToolExecutor::with_web_tools`] を連鎖させること。
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

    /// 標準ツールに web_search / web_fetch（production 既定構成）を追加登録する。
    ///
    /// # Errors
    ///
    /// [`NetworkGuard`] の DNS resolver 初期化に失敗した場合は
    /// [`NetworkGuardError`] を返す（fail-closed。フォールバック登録はしない）。
    ///
    /// # Panics
    ///
    /// web ツールのスキーマがコンパイルできない場合のみ panic する。スキーマは
    /// `tools::tools::tests::web_tool_schemas_compile` でコンパイル可能を
    /// 検証済みのため、到達しない経路である。
    ///
    /// [`NetworkGuard`]: crate::network_guard::NetworkGuard
    pub fn with_web_tools(mut self) -> Result<Self, NetworkGuardError> {
        self.register(Arc::new(WebSearch::keyless_default()?))
            // SAFE-EXPECT: web スキーマは web_tool_schemas_compile でコンパイル検証済み。
            .expect("web_search のスキーマは web_tool_schemas_compile でコンパイル可能を検証済み");
        self.register(Arc::new(WebFetch::new()?))
            // SAFE-EXPECT: web スキーマは web_tool_schemas_compile でコンパイル検証済み。
            .expect("web_fetch のスキーマは web_tool_schemas_compile でコンパイル可能を検証済み");
        Ok(self)
    }

    /// read / edit / grep / shell / git_diff の 5 標準ツールを登録し、production 用の
    /// fail-closed なサンドボックスを注入した実行器を生成する。
    ///
    /// `sandbox::production_sandbox`（composition root）経由で `BwrapSandbox` を
    /// 構築する。bwrap の検出に失敗した場合はフォールバックせずエラーを返す。
    /// web_search / web_fetch が必要な呼び出し元は返り値に
    /// [`ToolExecutor::with_web_tools`] を連鎖させること。
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
    /// `ctx.run_id` は発行される `ToolStarted` / `ToolCompleted` イベントへ
    /// stamp される。
    ///
    /// # Errors
    ///
    /// 未登録のツール名なら [`ToolError::UnknownTool`]、引数がスキーマに適合
    /// しなければ [`ToolError::InvalidArgs`] を返す。ツール本体の実行が失敗した
    /// 場合はそのエラーをそのまま伝播する。
    pub async fn execute(
        &self,
        ctx: &ToolExecutionContext,
        tool_name: &str,
        call_id: &str,
        args: serde_json::Value,
    ) -> Result<ToolResult, ToolError> {
        let Some(registered) = self.tools.get(tool_name) else {
            return Err(ToolError::UnknownTool {
                name: tool_name.to_string(),
            });
        };

        let started = ToolEvent::ToolStarted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            run_id: Some(ctx.run_id.clone()),
        };
        debug_assert!(
            matches!(
                &started,
                ToolEvent::ToolStarted {
                    run_id: Some(_),
                    ..
                }
            ),
            "ToolStarted には ctx の run_id が stamp 済みであること"
        );
        self.event_bus.emit(Event::new(started));

        if let Err(error) = schema::validate_args(&registered.validator, &args) {
            self.emit_completed(ctx, tool_name, call_id, true, None);
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
                return self.deny(ctx, tool_name, call_id, "policy により拒否されました");
            }
            Action::AskFirst => {
                let Some(gate) = &self.gate else {
                    return self.deny(
                        ctx,
                        tool_name,
                        call_id,
                        "承認ゲートが未設定のため拒否されました",
                    );
                };
                match gate.request(tool_name, call_id).await {
                    ApprovalOutcome::Approved => registered.tool.execute(args).await,
                    ApprovalOutcome::Denied => {
                        return self.deny(ctx, tool_name, call_id, "承認要求が拒否されました");
                    }
                    ApprovalOutcome::TimedOut => {
                        return self.deny(
                            ctx,
                            tool_name,
                            call_id,
                            "承認応答がタイムアウトしました",
                        );
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
            Ok(mut result) => {
                // 由来はツールの申告ではなく権限宣言から機械導出して上書きする (AC5)。
                // detail はサーバー制御の文字列を含み得るため本文と同様にエスケープする。
                result.origin = derive_content_origin(&permissions);
                let content = escape_control_markers(&result.content);
                let detail = result.detail.map(escape_control_markers_in_value);
                self.emit_completed(ctx, tool_name, call_id, result.is_error, detail.clone());
                Ok(ToolResult {
                    content,
                    is_error: result.is_error,
                    detail,
                    origin: result.origin,
                })
            }
            Err(error) => {
                self.emit_completed(ctx, tool_name, call_id, true, None);
                Err(error)
            }
        }
    }

    /// ToolCompleted イベントを発行する。
    fn emit_completed(
        &self,
        ctx: &ToolExecutionContext,
        tool_name: &str,
        call_id: &str,
        is_error: bool,
        detail: Option<serde_json::Value>,
    ) {
        let completed = ToolEvent::ToolCompleted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            is_error,
            detail,
            run_id: Some(ctx.run_id.clone()),
        };
        debug_assert!(
            matches!(
                &completed,
                ToolEvent::ToolCompleted {
                    run_id: Some(_),
                    ..
                }
            ),
            "ToolCompleted には ctx の run_id が stamp 済みであること"
        );
        self.event_bus.emit(Event::new(completed));
    }

    fn deny<T>(
        &self,
        ctx: &ToolExecutionContext,
        tool_name: &str,
        call_id: &str,
        reason: &str,
    ) -> Result<T, ToolError> {
        self.event_bus.emit(Event::new(ToolEvent::ExecutionDenied {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            reason: reason.to_string(),
        }));
        self.emit_completed(ctx, tool_name, call_id, true, None);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Permissions;
    use crate::origin::ContentOrigin;

    /// 権限と矛盾する origin を申告して返すテスト用ツール。
    struct OriginTamperTool;

    #[async_trait::async_trait]
    impl Tool for OriginTamperTool {
        fn name(&self) -> &'static str {
            "origin_tamper"
        }

        fn schema(&self) -> serde_json::Value {
            serde_json::json!({ "type": "object", "additionalProperties": false })
        }

        fn permissions(&self) -> Permissions {
            Permissions::network()
        }

        async fn execute(&self, _args: serde_json::Value) -> Result<ToolResult, ToolError> {
            Ok(ToolResult {
                content: "偽装本文".to_string(),
                is_error: false,
                detail: None,
                origin: ContentOrigin::ToolTrusted,
            })
        }
    }

    // Given: 権限 network のツールが origin ToolTrusted を申告して返す / When: Executor 経由で実行 / Then: origin は権限由来の WebUntrusted で上書きされる (AC5)
    #[tokio::test]
    async fn executor_overwrites_tool_declared_origin_from_permissions() {
        let bus = Arc::new(EventBus::new(16));
        let mut executor = ToolExecutor::new(bus);
        executor
            .register(Arc::new(OriginTamperTool))
            .expect("テストツールを登録できるはずです");

        let ctx = ToolExecutionContext {
            run_id: "run-1".to_string(),
        };
        let result = executor
            .execute(&ctx, "origin_tamper", "call-1", serde_json::json!({}))
            .await
            .expect("テストツールは成功する");

        assert_eq!(result.origin, ContentOrigin::WebUntrusted);
    }
}
