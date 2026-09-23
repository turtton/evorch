//! ツール実行の窓口（ToolExecutor）。
//!
//! 登録されたツールへの引数スキーマ検証、イベントバスへの開始・完了イベント
//! 発行、結果本文の制御マーカエスケープ（ADR 0008）を一貫して担う。ツールの
//! 実行は必ずこの Executor 経由で行うこと。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

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
use crate::tool::{Permissions, Tool, ToolExecutionMode};
use crate::tools::{Edit, GitDiff, Grep, Read, Shell, WebFetch, WebSearch, Write};

mod prepared;
mod specs;
pub use prepared::{PreparedToolCall, ValidatedToolCall};
pub use specs::ToolSpec;

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
    /// 実行元スレッドの識別子。スレッド相関が利用できない場合は `None`。
    pub thread_id: Option<String>,
    pub call_id: Option<String>,
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
    tools: HashMap<String, RegisteredTool>,
    /// ツール能力を実行操作へ分類する方針。
    policy: ApprovalPolicy,
    /// 利用者の承認応答を待つ任意のゲート。
    gate: Option<ApprovalGate>,
    default_cwd: RwLock<Option<std::path::PathBuf>>,
}

impl ToolExecutor {
    /// イベントバスを紐付いた空の実行器を生成する。
    pub fn new(event_bus: Arc<EventBus>) -> Self {
        Self {
            event_bus,
            tools: HashMap::new(),
            policy: ApprovalPolicy::allow_all(),
            gate: None,
            default_cwd: RwLock::new(None),
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
            .insert(tool.name().to_owned(), RegisteredTool { tool, validator });
        Ok(())
    }

    /// read / write / edit / grep / shell / git_diff の 6 標準ツールを登録した実行器を
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
        Self::with_standard_tools_in(event_bus, sandbox, None)
    }

    pub fn with_standard_tools_in(
        event_bus: Arc<EventBus>,
        sandbox: Arc<dyn Sandbox>,
        default_cwd: Option<std::path::PathBuf>,
    ) -> Self {
        let mut executor = Self::new(event_bus);
        let shell = Shell::new(Arc::clone(&sandbox));
        let shell = match default_cwd.clone() {
            Some(cwd) => shell.with_default_cwd(cwd),
            None => shell,
        };
        let standard: [Arc<dyn Tool>; 6] = [
            Arc::new(Read),
            Arc::new(Edit),
            Arc::new(Write),
            Arc::new(Grep),
            Arc::new(shell),
            Arc::new(GitDiff::new(sandbox)),
        ];
        for tool in standard {
            executor
                .register(tool)
                // SAFE-EXPECT: 標準スキーマは全件をクレート内テストでコンパイル検証している。
                .expect("標準ツールのスキーマは all_standard_tool_schemas_compile でコンパイル可能を検証済み");
        }
        if let Some(cwd) = default_cwd {
            executor.set_default_cwd(cwd);
        }
        executor
    }

    /// 登録済みの shell ツールがあれば、その既定作業ディレクトリを更新する。
    pub fn set_default_cwd(&self, cwd: std::path::PathBuf) {
        *self
            .default_cwd
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(cwd.clone());
        for registered in self.tools.values() {
            registered.tool.set_default_cwd(cwd.clone());
        }
    }

    /// Resolve relative file arguments against this run's workspace, never global cwd.
    fn scoped_args(&self, name: &str, mut args: serde_json::Value) -> serde_json::Value {
        let root = self
            .default_cwd
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let Some(root) = root.as_ref() else {
            return args;
        };
        let keys: &[&str] = match name {
            "read" => &["path", "file", "file_path", "filename", "target"],
            "write" | "edit" | "grep" => &["path"],
            "shell" | "git_diff" => &["cwd"],
            _ => &[],
        };
        if let Some(object) = args.as_object_mut() {
            for key in keys {
                if let Some(path) = object.get(*key).and_then(serde_json::Value::as_str) {
                    let path = std::path::Path::new(path);
                    if path.is_relative() {
                        object.insert(
                            (*key).into(),
                            root.join(path).to_string_lossy().into_owned().into(),
                        );
                    }
                }
            }
            if matches!(name, "shell" | "git_diff") && !object.contains_key("cwd") {
                object.insert("cwd".into(), root.to_string_lossy().into_owned().into());
            }
        }
        args
    }

    /// Cancel all yielded shell processes owned by this run. Call at every
    /// terminal/cancellation boundary, even if the executor is shared.
    pub fn cancel_shell_jobs(&self, run_id: &str) {
        if let Some(shell) = self.tools.get("shell") {
            shell.tool.cancel_shell_jobs(run_id);
        }
    }

    /// Wait for real process teardown before workspace cleanup. On error the
    /// caller must retain the workspace rather than deleting a live job's cwd.
    pub async fn drain_shell_jobs(&self, run_id: &str) -> Result<(), ToolError> {
        if let Some(shell) = self.tools.get("shell") {
            shell.tool.drain_shell_jobs(run_id).await
        } else {
            Ok(())
        }
    }

    /// Release terminal handles after durable side-effect markers have been saved.
    /// The caller must prevent a new incarnation of this run from starting first.
    pub fn release_shell_jobs(&self, run_id: &str) -> Result<(), ToolError> {
        if let Some(shell) = self.tools.get("shell") {
            shell.tool.release_shell_jobs(run_id)
        } else {
            Ok(())
        }
    }

    pub fn has_running_shell_jobs(&self, run_id: &str) -> bool {
        self.tools
            .get("shell")
            .is_some_and(|shell| shell.tool.has_running_shell_jobs(run_id))
    }

    pub fn has_unobserved_shell_jobs(&self, run_id: &str) -> bool {
        self.tools
            .get("shell")
            .is_some_and(|shell| shell.tool.has_unobserved_shell_jobs(run_id))
    }

    pub fn retain_shell_call_guard(
        &self,
        run_id: &str,
        call_id: &str,
        guard: Box<dyn Send + Sync>,
    ) {
        if let Some(shell) = self.tools.get("shell") {
            shell.tool.retain_shell_call_guard(run_id, call_id, guard);
        }
    }

    /// Retain the workspace snapshot guard while an async shell can mutate it.
    /// Control calls (poll/stdin/stop) keep normal tool authorization but must
    /// not reacquire that same mutation lock.
    pub fn retain_shell_job_guard(&self, run_id: &str, job_id: &str, guard: Box<dyn Send + Sync>) {
        if let Some(shell) = self.tools.get("shell") {
            shell.tool.retain_shell_job_guard(run_id, job_id, guard);
        }
    }

    /// 登録済み shell に、審査ゲートと承認時だけ使う非隔離経路を設定する。
    pub fn set_shell_escalation(
        &self,
        gate: Arc<dyn crate::tools::shell_escalation::ShellEscalationGate>,
        unsandboxed: Arc<dyn Sandbox>,
    ) {
        if let Some(registered) = self.tools.get("shell") {
            registered.tool.set_shell_escalation(gate, unsandboxed);
        }
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

    /// read / write / edit / grep / shell / git_diff の 6 標準ツールを登録し、production 用の
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
        let root = config.workspace_root().to_path_buf();
        let output_root =
            crate::output::output_root().map_err(|error| SandboxError::BwrapUnavailable {
                detail: format!("temporary output directory: {error}"),
            })?;
        Ok(Self::with_standard_tools_in(
            event_bus,
            sandbox::production_sandbox(config.ro_bind(output_root))?,
            Some(root),
        ))
    }

    /// 実行判定に使う承認方針を設定する。
    pub fn set_policy(&mut self, policy: ApprovalPolicy) -> &mut Self {
        self.policy = policy;
        self
    }

    /// 登録済みツールの権限宣言を返す。未登録なら None。
    pub fn tool_permissions(&self, tool_name: &str) -> Option<Permissions> {
        self.tools
            .get(tool_name)
            .map(|registered| registered.tool.permissions())
    }

    pub fn requires_scope_gate(&self, tool_name: &str) -> bool {
        self.tools
            .get(tool_name)
            .is_some_and(|registered| registered.tool.requires_scope_gate())
    }

    /// 登録済みツールの並行実行モードを返す。未登録なら排他実行とする。
    pub fn tool_execution_mode(&self, tool_name: &str) -> ToolExecutionMode {
        self.tools
            .get(tool_name)
            .map_or(ToolExecutionMode::Exclusive, |registered| {
                registered.tool.execution_mode()
            })
    }

    /// loop 側 3 層 AND 判定の per-tool 層入力として、登録済みツールの実行分類を返す。
    /// 未登録なら None。executor 自身も同じ policy で判定するため、allow_all 以外の
    /// policy を設定した構成では loop 側と executor 側の二重 ask になりうる点に注意。
    pub fn classify_tool(&self, tool_name: &str) -> Option<sandbox::PolicyDecision> {
        let permissions = self.tool_permissions(tool_name)?;
        Some(
            self.policy
                .classify(tool_name, &capabilities_of(&permissions)),
        )
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
        let ctx = ToolExecutionContext {
            call_id: Some(call_id.into()),
            ..ctx.clone()
        };
        self.execute_inner(&ctx, tool_name, call_id, args, None)
            .await
    }

    async fn execute_inner(
        &self,
        ctx: &ToolExecutionContext,
        tool_name: &str,
        call_id: &str,
        args: serde_json::Value,
        authorized: Option<Action>,
    ) -> Result<ToolResult, ToolError> {
        let Some(registered) = self.tools.get(tool_name) else {
            return Err(ToolError::UnknownTool {
                name: tool_name.to_string(),
            });
        };

        let args = self.scoped_args(tool_name, args);
        let started = ToolEvent::ToolStarted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            input: Some(args.clone()),
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
            self.emit_completed(ctx, tool_name, call_id, Err(&error));
            return Err(error);
        }

        let permissions = registered.tool.permissions();
        let capabilities = capabilities_of(&permissions);
        let action = authorized.unwrap_or_else(|| {
            resolve(
                self.policy.classify(tool_name, &capabilities),
                self.policy.mode(),
            )
        });
        let outcome = match action {
            Action::Proceed => registered.tool.execute_with_context(ctx, args).await,
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
                match gate
                    .request_with_input(tool_name, call_id, Some(args.clone()))
                    .await
                {
                    ApprovalOutcome::Approved => {
                        registered.tool.execute_with_context(ctx, args).await
                    }
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
                let first = registered
                    .tool
                    .execute_with_context(ctx, args.clone())
                    .await;
                if !is_failure(&first) {
                    first
                } else if let Some(gate) = &self.gate {
                    match gate
                        .request_with_input(
                            tool_name,
                            &approval_id(ctx, call_id),
                            Some(args.clone()),
                        )
                        .await
                    {
                        ApprovalOutcome::Approved => {
                            registered.tool.execute_with_context(ctx, args).await
                        }
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
                result = crate::output::limit_result(result);
                result.origin = derive_content_origin(&permissions);
                let content = escape_control_markers(&result.content);
                let detail = result.detail.map(escape_control_markers_in_value);
                let result = ToolResult {
                    content,
                    is_error: result.is_error,
                    detail,
                    origin: result.origin,
                };
                self.emit_completed(ctx, tool_name, call_id, Ok(&result));
                Ok(result)
            }
            Err(error) => {
                self.emit_completed(ctx, tool_name, call_id, Err(&error));
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
        result: Result<&ToolResult, &ToolError>,
    ) {
        let completed = ToolEvent::ToolCompleted {
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            is_error: result.map_or(true, |result| result.is_error),
            output: Some(match result {
                Ok(result) => result.content.clone(),
                Err(error) => escape_control_markers(&error.to_string()),
            }),
            detail: result.ok().and_then(|result| result.detail.clone()),
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
        let error = ToolError::ExecutionDenied {
            tool_name: tool_name.to_string(),
            reason: reason.to_string(),
        };
        self.emit_completed(ctx, tool_name, call_id, Err(&error));
        Err(error)
    }
}

fn capabilities_of(permissions: &Permissions) -> Capabilities {
    Capabilities {
        fs_read: permissions.fs_read,
        fs_write: permissions.fs_write,
        process_spawn: permissions.process_spawn,
        network: permissions.network,
    }
}

fn approval_id(ctx: &ToolExecutionContext, call_id: &str) -> String {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let attempt = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}:{call_id}:{attempt}", ctx.run_id)
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

    #[tokio::test]
    async fn executor_emits_input_and_output_for_read() {
        // Given: a real read tool, file, and subscribed event bus.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sample.txt");
        std::fs::write(&path, "本文\n").unwrap();
        let input = serde_json::json!({"path": path});
        let bus = Arc::new(EventBus::new(16));
        let mut receiver = bus.subscribe();
        let mut executor = ToolExecutor::new(bus);
        executor.register(Arc::new(Read)).unwrap();
        let ctx = ToolExecutionContext {
            run_id: "r1".into(),
            thread_id: None,
            call_id: None,
        };

        // When: the executor reads the file.
        let result = executor
            .execute(&ctx, "read", "c1", input.clone())
            .await
            .unwrap();

        // Then: subscribers receive the input and returned file content separately.
        let started = serde_json::to_value(receiver.recv().await.unwrap()).unwrap();
        let completed = serde_json::to_value(receiver.recv().await.unwrap()).unwrap();
        assert_eq!(started["kind"]["payload"]["payload"]["input"], input);
        assert_eq!(
            completed["kind"]["payload"]["payload"]["output"],
            result.content
        );
        assert!(result.content.contains("本文"));
    }

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
            thread_id: None,
            call_id: None,
        };
        let result = executor
            .execute(&ctx, "origin_tamper", "call-1", serde_json::json!({}))
            .await
            .expect("テストツールは成功する");

        assert_eq!(result.origin, ContentOrigin::WebUntrusted);
    }
}
