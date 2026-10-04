//! delegate メタ操作のハンドラ。

use config::agent_categories::CategoryId;
use serde::Deserialize;

use super::{DispatchResult, error, parse, parse_category, parse_role, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, InterruptKind, RunConfig, WorkspaceMode};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DelegateArgs {
    #[serde(default)]
    task: Option<crate::team::TaskSpec>,
    #[serde(default)]
    images: Vec<crate::run::DelegateImage>,
    target: DelegateTarget,
    prompt: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    workspace_mode: Option<WorkspaceMode>,
    #[serde(default)]
    workspace_branch: Option<String>,
    #[serde(default)]
    load_skills: Vec<String>,
}

/// Stable public role order shared by the schema and runtime validation.
pub(super) const DELEGATE_ROLES: &[&str] = &[
    "orchestrator",
    "explorer",
    "worker",
    "reviewer",
    "web_researcher",
    "planner",
    "oracle",
    "multimodal_looker",
    "multimodallooker",
];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DelegateTarget {
    role: String,
    #[serde(default, deserialize_with = "present_category")]
    category: Option<String>,
}

fn present_category<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    String::deserialize(deserializer).map(Some)
}

impl DelegateTarget {
    fn resolve(self) -> Result<(agents::Role, Option<String>), String> {
        if !DELEGATE_ROLES.contains(&self.role.as_str()) {
            return Err(serde_json::json!({
                "code":"unknown_role", "role":self.role,
                "message":format!("unknown target.role: {}", self.role)
            })
            .to_string());
        }
        let role = parse_role(&self.role)?;
        let Some(category) = self.category else {
            return Ok((role, None));
        };
        let categories: Vec<_> = config::agent_categories::public_categories()
            .filter(|category| category.role == self.role)
            .map(|category| category.id)
            .collect();
        if categories.is_empty() {
            return Err(format!(
                "target.role={} has no public categories; omit target.category. Use target={{\"role\":\"{}\"}}. For plan review use target={{\"role\":\"reviewer\",\"category\":\"{}\"}}.",
                self.role,
                self.role,
                CategoryId::PlanReview
            ));
        }
        let category = parse_category(&category)?;
        if !categories.contains(&category) {
            let owner = category.role();
            return Err(format!(
                "target.category `{category}` is only valid for target.role={owner}. Use target={{\"role\":\"{owner}\",\"category\":\"{category}\"}}, or omit target.category to use the {} base binding.",
                self.role
            ));
        }
        Ok((role, Some(category.to_string())))
    }
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
    // A child may ask for a decision before it can finish. Use the same
    // attention-aware wait exposed to the model, so the parent can answer it.
    let result = super::runs::waiting::wait(
        state,
        runtime,
        serde_json::json!({"run_id": child.to_string()}),
    )
    .await;
    if result.result.is_error && state.interrupted() != Some(InterruptKind::Stop) {
        cleanup_delegates(runtime, std::iter::once(child)).await;
    }
    result
}

pub(crate) fn spawn_delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> Result<crate::RunId, DispatchResult> {
    if input.get("role").is_some() || input.get("category").is_some() {
        return Err(error(format!(
            "invalid arguments: put role and category inside the required target object. For planning use target={{\"role\":\"planner\"}}; for plan review use target={{\"role\":\"reviewer\",\"category\":\"{}\"}}.",
            CategoryId::PlanReview
        )));
    }
    let args = match parse::<DelegateArgs>(input) {
        Ok(args) => args,
        Err(message) => return Err(error(message)),
    };
    if args.interactive && !args.background {
        return Err(error(
            "invalid arguments: interactive=true requires background=true",
        ));
    }
    let (role, category) = match args.target.resolve() {
        Ok(target) => target,
        Err(message) => return Err(error(message)),
    };
    if !args.images.is_empty() && role != agents::Role::MultimodalLooker {
        return Err(error("image payload requires MultimodalLooker"));
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
    let run_ids = children
        .iter()
        .filter_map(|child| child.as_ref().ok())
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    if run_ids.is_empty() {
        return children.into_iter().filter_map(Result::err).collect();
    }
    // One wait covers the whole wave and returns when any child needs a
    // decision, while preserving each delegate call's own tool result.
    let waited = super::runs::waiting::wait(
        state,
        runtime,
        serde_json::json!({"run_ids":run_ids, "mode":"all"}),
    )
    .await;
    if waited.result.is_error {
        if state.interrupted() != Some(InterruptKind::Stop) {
            cleanup_delegates(
                runtime,
                children
                    .iter()
                    .filter_map(|child| child.as_ref().ok().copied()),
            )
            .await;
        }
        let message = waited.result.content;
        return children
            .into_iter()
            .map(|child| child.err().unwrap_or_else(|| error(message.clone())))
            .collect();
    }
    let snapshot: serde_json::Value = match serde_json::from_str(&waited.result.content) {
        Ok(snapshot) => snapshot,
        Err(parse_error) => {
            return children
                .into_iter()
                .map(|child| {
                    child.err().unwrap_or_else(|| {
                        error(format!("invalid child wait result: {parse_error}"))
                    })
                })
                .collect();
        }
    };
    children
        .into_iter()
        .map(|child| match child {
            Err(result) => result,
            Ok(run_id) => {
                let run = snapshot["runs"]
                    .as_array()
                    .and_then(|runs| runs.iter().find(|run| run["run_id"] == run_id.to_string()));
                match run {
                    Some(run)
                        if run["status"] != "still_running"
                            && run["has_pending_question"] != true =>
                    {
                        success(run["phase"].as_str().unwrap_or("Error"))
                    }
                    Some(run) => super::serialize(&serde_json::json!({
                        "run_id": run_id.to_string(),
                        "phase": run["phase"],
                        "needs_user_input": run["needs_user_input"],
                        "has_pending_question": run["has_pending_question"],
                        "inbox_ready": snapshot["inbox_ready"],
                        "user_input_ready": snapshot["user_input_ready"],
                        "timed_out": snapshot["timed_out"],
                    })),
                    None => error(format!("missing child wait result for {run_id}")),
                }
            }
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
