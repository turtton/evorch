//! delegate メタ操作のハンドラ。

use event_bus::AgentRunPhase;
use serde::Deserialize;

use super::{DispatchResult, error, parse, parse_category, parse_role, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, RunConfig, WorkspaceMode};

#[derive(Deserialize)]
struct DelegateArgs {
    #[serde(default)]
    task: Option<crate::team::TaskSpec>,
    #[serde(default)]
    images: Vec<crate::run::DelegateImage>,
    role: Option<String>,
    prompt: String,
    #[serde(default)]
    background: bool,
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

pub(super) async fn delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> DispatchResult {
    let child = match spawn_delegate(state, runtime, input) {
        Ok(child) => child,
        Err(result) => return result,
    };
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        cleanup_delegates(runtime, std::iter::once(child)).await;
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

pub(crate) fn spawn_delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> Result<crate::RunId, DispatchResult> {
    let args = match parse::<DelegateArgs>(input) {
        Ok(args) => args,
        Err(message) => return Err(error(message)),
    };
    if args.interactive && !args.background {
        return Err(error(
            "invalid arguments: interactive=true requires background=true",
        ));
    }
    let role = match parse_role(args.role.as_deref().unwrap_or("worker")) {
        Ok(role) => role,
        Err(message) => return Err(error(message)),
    };
    if !args.images.is_empty() && role != agents::Role::MultimodalLooker {
        return Err(error("image payload requires MultimodalLooker"));
    }
    let category = match parse_args_category(args.category) {
        Ok(category) => category,
        Err(message) => return Err(error(message)),
    };
    if category.is_some() && role != agents::Role::Worker {
        return Err(error("category is only valid for role=worker"));
    }
    let load_skills = match validate_load_skills(state, &args.load_skills) {
        Ok(load_skills) => load_skills,
        Err(message) => return Err(error(message)),
    };
    let config = RunConfig {
        team_task: args.task,
        interactive: args.interactive,
        images: args.images,
        name: args.name,
        category,
        load_skills,
        workspace_mode: args.workspace_mode.unwrap_or_default(),
        workspace_branch: args.workspace_branch,
        ..RunConfig::default()
    };
    if args.background {
        return Err(
            match runtime.delegate_background_as_child(
                state.caller_run_id(),
                role,
                args.prompt,
                config,
            ) {
                Ok(run_id) => {
                    runtime.attach_goal_child(state.caller_run_id(), run_id, role);
                    success(run_id.to_string())
                }
                Err(runtime_error) => error(runtime_error.to_string()),
            },
        );
    }
    let child =
        match runtime.delegate_awaited_child(state.caller_run_id(), (role, args.prompt, config)) {
            Ok(child) => child,
            Err(runtime_error) => return Err(error(runtime_error.to_string())),
        };
    runtime.attach_goal_child(state.caller_run_id(), child, role);
    state.emit_delegated(&state.caller_run_id().to_string(), &child.to_string());
    Ok(child)
}

pub(crate) async fn wait_delegates(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    children: Vec<Result<crate::RunId, DispatchResult>>,
) -> Vec<DispatchResult> {
    if !children.iter().any(Result::is_ok) {
        return children.into_iter().filter_map(Result::err).collect();
    }
    if state.transition(AgentRunPhase::Waiting, None).is_err() {
        cleanup_delegates(
            runtime,
            children
                .iter()
                .filter_map(|child| child.as_ref().ok().copied()),
        )
        .await;
        return children
            .into_iter()
            .map(|child| {
                child
                    .err()
                    .unwrap_or_else(|| error("parent run could not enter Waiting"))
            })
            .collect();
    }
    let cancel = state.channels.cancel_rx.clone();
    let results = futures_util::future::join_all(children.into_iter().map(|child| {
        let mut cancel = cancel.clone();
        async move {
            match child {
                Ok(child) => Ok(tokio::select! {
                    biased;
                    _ = async { drop(cancel.wait_for(|cancelled| *cancelled).await); } => {
                        match runtime.cancel(child) {
                            Ok(()) => runtime.wait(child).await,
                            Err(error) => Err(error),
                        }
                    }
                    result = runtime.wait(child) => result,
                }),
                Err(result) => Err(result),
            }
        }
    }))
    .await;
    let resumed = state.transition(AgentRunPhase::Running, None).is_ok();
    results
        .into_iter()
        .map(|result| match result {
            Err(result) => result,
            Ok(_) if !resumed => error("parent run could not resume Running"),
            Ok(Ok(phase)) => success(format!("{phase:?}")),
            Ok(Err(runtime_error)) => error(runtime_error.to_string()),
        })
        .collect()
}

pub(crate) async fn cleanup_delegates(
    runtime: &AgentRuntime,
    children: impl IntoIterator<Item = crate::RunId>,
) {
    let waits: Vec<_> = children
        .into_iter()
        .map(|child| {
            if let Err(error) = runtime.cancel(child) {
                tracing::warn!(%child, %error, "delegate cleanup cancellation failed");
            }
            async move {
                if let Err(error) = runtime.wait(child).await {
                    tracing::warn!(%child, %error, "delegate cleanup wait failed");
                }
            }
        })
        .collect();
    futures_util::future::join_all(waits).await;
}
