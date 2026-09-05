//! 委譲系メタ操作 (delegate / delegate_background) のハンドラ。

use event_bus::AgentRunPhase;
use serde::Deserialize;

use super::{DispatchResult, error, parse, parse_category, parse_role, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunConfig, WorkspaceMode};

#[derive(Deserialize)]
struct DelegateBackgroundArgs {
    role: String,
    prompt: String,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    workspace_mode: Option<WorkspaceMode>,
    #[serde(default)]
    workspace_branch: Option<String>,
    #[serde(default)]
    load_skills: Vec<String>,
}

#[derive(Deserialize)]
struct DelegateArgs {
    role: String,
    prompt: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    category: Option<String>,
    #[serde(default)]
    workspace_mode: Option<WorkspaceMode>,
    #[serde(default)]
    workspace_branch: Option<String>,
    #[serde(default)]
    load_skills: Vec<String>,
}

fn parse_args_category(category: Option<String>) -> Result<Option<String>, String> {
    category.as_deref().map(parse_category).transpose()
}

/// load_skills を検証し、重複を除去した注入名リストを返す (issue #53 / AC6)。
///
/// 空ならそのまま空を返す (レジストリ参照も行わない)。空でない場合は、
/// 子 run の生成・モデル呼び出しより前に fail-closed で検証する: レジストリ
/// 未接続なら "not configured"、未知の名前なら "unknown skill" を運ぶエラー。
/// 重複は最初の出現位置を保持して除去する。
fn validate_load_skills(state: &LoopState, names: &[String]) -> Result<Vec<String>, String> {
    let mut unique: Vec<String> = Vec::with_capacity(names.len());
    for name in names {
        if !unique.contains(name) {
            unique.push(name.clone());
        }
    }
    if unique.is_empty() {
        return Ok(unique);
    }
    let Some(registry) = state.skills() else {
        return Err("skill registry is not configured".to_string());
    };
    for name in &unique {
        if registry.get(name).is_none() {
            return Err(format!("unknown skill: {name}"));
        }
    }
    Ok(unique)
}

pub(super) fn delegate_background(
    state: &LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<DelegateBackgroundArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let role = match parse_role(&args.role) {
        Ok(role) => role,
        Err(message) => return error(message),
    };
    let category = match parse_args_category(args.category) {
        Ok(category) => category,
        Err(message) => return error(message),
    };
    let load_skills = match validate_load_skills(state, &args.load_skills) {
        Ok(load_skills) => load_skills,
        Err(message) => return error(message),
    };
    match runtime.delegate_background_as_child(
        state.caller_run_id(),
        role,
        args.prompt,
        RunConfig {
            interactive: args.interactive,
            name: args.name,
            category,
            load_skills,
            workspace_mode: args.workspace_mode.unwrap_or_default(),
            workspace_branch: args.workspace_branch,
            ..RunConfig::default()
        },
    ) {
        Ok(run_id) => {
            runtime.attach_goal_child(state.caller_run_id(), run_id, role);
            success(run_id.to_string())
        }
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}

pub(super) async fn delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let args = match parse::<DelegateArgs>(input) {
        Ok(args) => args,
        Err(message) => return error(message),
    };
    let role = match parse_role(&args.role) {
        Ok(role) => role,
        Err(message) => return error(message),
    };
    let category = match parse_args_category(args.category) {
        Ok(category) => category,
        Err(message) => return error(message),
    };
    let load_skills = match validate_load_skills(state, &args.load_skills) {
        Ok(load_skills) => load_skills,
        Err(message) => return error(message),
    };
    let child = match runtime.delegate_background_as_child(
        state.caller_run_id(),
        role,
        args.prompt,
        RunConfig {
            name: args.name,
            category,
            load_skills,
            workspace_mode: args.workspace_mode.unwrap_or_default(),
            workspace_branch: args.workspace_branch,
            ..RunConfig::default()
        },
    ) {
        Ok(child) => child,
        Err(runtime_error) => return error(runtime_error.to_string()),
    };
    runtime.attach_goal_child(state.caller_run_id(), child, role);
    state.emit_delegated(&state.caller_run_id().to_string(), &child.to_string());
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        return error("parent run could not enter Waiting");
    }
    let result = runtime.wait(child).await;
    if state.transition(AgentRunPhase::Running, None).is_err() {
        return error("parent run could not resume Running");
    }
    match result {
        Ok(phase) => success(format!("{phase:?}")),
        Err(runtime_error) => error(runtime_error.to_string()),
    }
}
