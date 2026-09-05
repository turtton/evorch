//! headless 実行エントリ (issue #79)。
//!
//! provider composition root ([`runtime::compose_runtime`]) を CLI から共有する
//! 最小実行経路を提供する。

use std::path::PathBuf;
use std::sync::Arc;

use event_bus::{AgentRunPhase, EventBus};
use routing::EnvLookup;
use runtime::{
    ExecutionPolicy, ModelSource, Role, RunConfig, RunId, RuntimeComposition, compose_runtime,
    production_executor,
};
use sandbox::credential::{CredentialStore, open_default};
use sandbox::{CredentialError, DirectSandbox};
use tools::ToolExecutor;

/// `parse_args` の使い方テキスト。
pub const USAGE: &str = "usage: evorch run --project <dir> --role <worker|orchestrator|explorer|reviewer> --prompt <text> [--user-config <dir>]";

/// `evorch run` の引数。
#[derive(Debug, Clone)]
pub struct HeadlessArgs {
    pub project_dir: PathBuf,
    pub role: Role,
    pub prompt: String,
    pub user_config_dir: Option<PathBuf>,
}

/// tool 実行の sandbox 方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxChoice {
    /// production 構成 (`production_executor`、fail-closed)。
    Production,
    /// 隔離なしの実行 (非 production / テスト専用の明示的 opt-out)。
    DirectUnchecked,
}

/// headless run の結果。
#[derive(Debug)]
pub struct HeadlessOutcome {
    pub run_id: RunId,
    pub phase: AgentRunPhase,
    pub final_text: Option<String>,
}

/// headless 実行の失敗。
#[derive(Debug, thiserror::Error)]
pub enum HeadlessError {
    #[error("{0}")]
    Usage(String),
    #[error(transparent)]
    Config(#[from] config::ConfigError),
    #[error(transparent)]
    Composition(#[from] runtime::CompositionError),
    #[error(transparent)]
    Runtime(#[from] runtime::RuntimeError),
    #[error(transparent)]
    Credential(#[from] CredentialError),
}

/// コマンドライン引数を手でパースする (clap 不使用)。
///
/// # Errors
/// サブコマンドが `run` 以外、フラグが未知、必須フラグやその値が欠落、
/// role が既知名でない場合に [`HeadlessError::Usage`] を返す。
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<HeadlessArgs, HeadlessError> {
    let mut argv = argv;
    match argv.next() {
        Some(command) if command == "run" => {}
        _ => return Err(usage_error()),
    }

    let mut project = None;
    let mut role = None;
    let mut prompt = None;
    let mut user_config = None;
    while let Some(flag) = argv.next() {
        let Some(value) = argv.next() else {
            return Err(usage_error());
        };
        match flag.as_str() {
            "--project" => project = Some(value),
            "--role" => role = Some(value),
            "--prompt" => prompt = Some(value),
            "--user-config" => user_config = Some(value),
            _ => return Err(usage_error()),
        }
    }

    let (Some(project_dir), Some(role_text), Some(prompt)) = (project, role, prompt) else {
        return Err(usage_error());
    };
    let Some(role) = parse_role(&role_text) else {
        return Err(usage_error());
    };

    Ok(HeadlessArgs {
        project_dir: PathBuf::from(project_dir),
        role,
        prompt,
        user_config_dir: user_config.map(PathBuf::from),
    })
}

fn usage_error() -> HeadlessError {
    HeadlessError::Usage(USAGE.to_string())
}

fn parse_role(text: &str) -> Option<Role> {
    match text.to_ascii_lowercase().as_str() {
        "orchestrator" => Some(Role::Orchestrator),
        "explorer" => Some(Role::Explorer),
        "worker" => Some(Role::Worker),
        "reviewer" => Some(Role::Reviewer),
        _ => None,
    }
}

/// headless run を 1 回実行する。
///
/// `Config::load` → `EventBus` → executor → credential store →
/// [`compose_runtime`] → delegate → `wait` の単一経路で構成し、
/// provider composition root を将来の GUI 経路と共有する。
///
/// # Errors
/// 設定読み込み・composition・sandbox 構築・実行の失敗時に
/// [`HeadlessError`] の対応 variant を返す。
pub async fn run_headless(
    args: HeadlessArgs,
    env: Arc<dyn EnvLookup>,
    sandbox: SandboxChoice,
) -> Result<HeadlessOutcome, HeadlessError> {
    let config = config::Config::load(&config::LoadOptions {
        project_dir: Some(args.project_dir.clone()),
        user_config_dir: args.user_config_dir.clone(),
        read_env: false,
        ..config::LoadOptions::default()
    })?;
    let bus = Arc::new(EventBus::new(256));
    let executor: Arc<ToolExecutor> = match sandbox {
        SandboxChoice::Production => production_executor(
            Arc::clone(&bus),
            &ExecutionPolicy::for_role(args.role),
            args.project_dir.clone(),
        )?,
        SandboxChoice::DirectUnchecked => Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        )),
    };
    let credential_store = open_credential_store(&args)?;

    let composed = compose_runtime(RuntimeComposition {
        config: &config,
        bus,
        executor,
        credential_store,
        env,
        model_source: ModelSource::Configured,
    })?;

    let run_id = composed
        .runtime
        .delegate_background(args.role, args.prompt, RunConfig::default());
    let phase = composed.runtime.wait(run_id).await?;
    let final_text = composed.runtime.run_result(run_id)?;

    Ok(HeadlessOutcome {
        run_id,
        phase,
        final_text,
    })
}

fn open_credential_store(args: &HeadlessArgs) -> Result<Arc<dyn CredentialStore>, HeadlessError> {
    let dir = args
        .user_config_dir
        .clone()
        .or_else(config::user_config_dir)
        .map(|dir| dir.join("credentials"))
        .ok_or_else(|| {
            HeadlessError::Usage(
                "cannot resolve a user config directory for the credential store; pass --user-config <dir>"
                    .to_string(),
            )
        })?;
    Ok(open_default(dir)?)
}
