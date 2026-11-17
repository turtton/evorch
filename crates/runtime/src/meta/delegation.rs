//! delegate メタ操作のハンドラ。

use serde::Deserialize;

use super::{DispatchResult, error, parse, parse_category, parse_role, success};
use crate::agent_loop::LoopState;
use crate::{AgentRuntime, InterruptKind, RunConfig};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DelegateArgs {
    #[serde(default)]
    task: Option<crate::team::TaskSpec>,
    target: DelegateTarget,
    prompt: String,
    #[serde(default)]
    background: bool,
    #[serde(default)]
    interactive: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    load_skills: Vec<String>,
}

/// Stable public role order shared by the schema and runtime validation.
/// Orchestrator is root-only and is never a delegate target (ADR 0044 §2).
pub(super) const DELEGATE_ROLES: &[&str] = &[
    "explorer",
    "worker",
    "reviewer",
    "web_researcher",
    "planner",
    "advisor",
    "oracle", // Legacy alias, like multimodallooker; normalized by parse_role.
    "multimodal_looker",
    "multimodallooker",
    "evaluator",
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
        // Reject Orchestrator under any spelling before the registry lookup, so the
        // caller learns why, rather than that the role is unknown.
        if parse_role(&self.role).is_ok_and(|role| role == agents::Role::Orchestrator) {
            return Err(serde_json::json!({
                "code":"role_not_delegable", "role":self.role,
                "message":"target.role=orchestrator cannot be delegated; Orchestrator is root-only. Delegate single-role work to another role."
            })
            .to_string());
        }
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
            let owner = parse_category(&category).map_or_else(
                |_| String::new(),
                |category| {
                    format!(
                        " Category `{category}` belongs to target.role={}.",
                        category.role()
                    )
                },
            );
            return Err(format!(
                "target.role={} has no public categories; omit target.category. Use target={}.{owner}",
                self.role,
                serde_json::json!({"role": self.role}),
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

/// Rejects top-level role/category, echoing the caller's own values as a target.
fn misplaced_target_error(input: &serde_json::Value) -> String {
    let message = "invalid arguments: put role and category inside the required target object.";
    let role = input.get("role").and_then(serde_json::Value::as_str);
    let category = input
        .get("category")
        .and_then(serde_json::Value::as_str)
        .and_then(|category| parse_category(category).ok());
    let target = match (role, category) {
        (_, Some(category)) => {
            serde_json::json!({"role": category.role(), "category": category.as_str()})
        }
        (Some(role), None) if DELEGATE_ROLES.contains(&role) => serde_json::json!({"role": role}),
        _ => return message.to_owned(),
    };
    format!("{message} Use target={target}.")
}

/// load_skills を検証し、重複を除去した注入名リストを返す (issue #53 / AC6)。
///
/// 空ならそのまま空を返す (レジストリ参照も行わない)。空でない場合は、
/// 子 run の生成・モデル呼び出しより前に fail-closed で検証する: レジストリ
/// 未接続なら "not configured"、未知の名前なら "unknown skill" (未信頼プロジェクトで
/// 除外した名前ならその理由と対処) を運ぶエラー。
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
            return Err(registry.unknown_skill_message(name));
        }
        if let Some(reason) = state.revoked_skill(name) {
            return Err(reason);
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
    let result = super::runs::waiting::wait_for_delegates(
        state,
        runtime,
        serde_json::json!({"run_id": child.to_string()}),
    )
    .await;
    if result.result.is_error && state.interrupted() != Some(InterruptKind::Stop) {
        cleanup_delegates(runtime, std::iter::once(child)).await;
    }
    project_direct_wait_result(result)
}

fn project_direct_wait_result(result: DispatchResult) -> DispatchResult {
    // Direct dispatch goes through this handler rather than wait_delegates;
    // both paths return the same output-bearing idle-turn snapshot.
    if !result.result.is_error
        && let Ok(snapshot) = serde_json::from_str::<serde_json::Value>(&result.result.content)
        && let Some(run) = snapshot["runs"].as_array().and_then(|runs| runs.first())
        && run["status"] == "turn_completed"
    {
        let mut value = run.clone();
        // Keep every global observation paired with the fields actually shown.
        // A simultaneous shell/lane/question event must not be silently ACKed
        // when the direct delegate flattens its child's idle-turn result.
        for field in [
            "inbox_ready",
            "messages",
            "user_input_ready",
            "answer_ready",
            "question_ids",
            "attention_question_ids",
            "events",
            "shell_jobs",
        ] {
            value[field] = snapshot[field].clone();
        }
        let mut dispatch = super::serialize(&value);
        dispatch.result.progress = result.result.progress;
        dispatch.observations = result.observations;
        return dispatch;
    }
    result
}

// The error is a whole tool result, produced once on a refused spawn; boxing it would
// only add an allocation to a path that is already the exception.
#[allow(clippy::result_large_err)]
pub(crate) fn spawn_delegate(
    state: &mut LoopState,
    runtime: &AgentRuntime,
    input: serde_json::Value,
) -> Result<crate::RunId, DispatchResult> {
    let _external = runtime
        .external_admission_guard()
        .map_err(|e| error(e.to_string()))?;
    if input.get("role").is_some() || input.get("category").is_some() {
        return Err(error(misplaced_target_error(&input)));
    }
    let args = match parse::<DelegateArgs>(input) {
        Ok(args) => args,
        Err(message) => return Err(error(message)),
    };
    let target = format!(
        "role={} category={}",
        args.target.role,
        args.target.category.as_deref().unwrap_or("-")
    );
    let (role, category) = match args.target.resolve() {
        Ok(target) => target,
        Err(message) => return Err(error(message)),
    };
    let load_skills = match validate_load_skills(state, &args.load_skills) {
        Ok(load_skills) => load_skills,
        Err(message) => return Err(error(message)),
    };
    let config = RunConfig {
        team_task: args.task,
        interactive: args.interactive,
        name: args.name,
        category,
        allowed_skill_roots: state.allowed_skill_roots(&load_skills),
        load_skills,
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
                    state.record_background_delegate(run_id.to_string(), target);
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
    let mut waited = super::runs::waiting::wait_for_delegates(
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
    let mut first = true;
    children
        .into_iter()
        .map(|child| match child {
            Err(result) => result,
            Ok(run_id) => {
                let run = snapshot["runs"]
                    .as_array()
                    .and_then(|runs| runs.iter().find(|run| run["run_id"] == run_id.to_string()));
                let interrupted = snapshot["events"].as_array().is_some_and(|events| {
                    events
                        .iter()
                        .any(|e| e["kind"] != "run_completed" && e["kind"] != "turn_completed")
                }) || snapshot["inbox_ready"] == true
                    || snapshot["user_input_ready"] == true;
                let mut dispatch = match run {
                    Some(run)
                        if matches!(
                            run["status"].as_str(),
                            Some("completed" | "cancelled" | "stopped" | "failed")
                        ) && run["has_pending_question"] != true
                            && !interrupted =>
                    {
                        success(run["phase"].as_str().unwrap_or("Error"))
                    }
                    Some(run) if run["status"] == "turn_completed" => {
                        let mut value = run.clone();
                        if interrupted {
                            for field in [
                                "inbox_ready",
                                "user_input_ready",
                                "answer_ready",
                                "question_ids",
                                "attention_question_ids",
                                "events",
                                "shell_jobs",
                            ] {
                                value[field] = snapshot[field].clone();
                            }
                        }
                        super::serialize(&value)
                    }
                    Some(run) => super::serialize(&serde_json::json!({
                        "run_id": run_id.to_string(),
                        "phase": run["phase"],
                        "needs_user_input": run["needs_user_input"],
                        "has_pending_question": run["has_pending_question"],
                        "inbox_ready": snapshot["inbox_ready"],
                        "user_input_ready": snapshot["user_input_ready"],
                        "answer_ready": snapshot["answer_ready"],
                        "question_ids": snapshot["question_ids"],
                        "attention_question_ids": snapshot["attention_question_ids"],
                        "events": snapshot["events"],
                        "shell_jobs": snapshot["shell_jobs"],
                    })),
                    None => error(format!("missing child wait result for {run_id}")),
                };
                if !dispatch.result.is_error {
                    dispatch.observations.child_progress = waited
                        .observations
                        .child_progress
                        .iter()
                        .filter(|progress| progress.run_id == run_id.to_string())
                        .cloned()
                        .collect();
                }
                if run.is_some_and(|r| r["status"] == "turn_completed") {
                    dispatch.observations.completions = waited
                        .observations
                        .completions
                        .iter()
                        .filter(|n| n.child == run_id)
                        .cloned()
                        .collect();
                }
                if first && interrupted && !dispatch.result.is_error {
                    dispatch.observations.questions =
                        std::mem::take(&mut waited.observations.questions);
                    dispatch.observations.events = std::mem::take(&mut waited.observations.events);
                    dispatch.observations.shell = std::mem::take(&mut waited.observations.shell);
                    first = false;
                }
                dispatch
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_idle_projection_keeps_global_payload_with_its_receipts() {
        let snapshot = serde_json::json!({
            "runs": [{"run_id":"child", "status":"turn_completed", "output":"child body", "turn_generation":7}],
            "inbox_ready":true, "user_input_ready":true, "answer_ready":true,
            "messages":[{"message_id":"message", "sender_run_id":"parent", "recipient_run_id":"caller",
                         "kind":"send", "content":"ordinary message", "reply_to":null}],
            "question_ids":["answer"], "attention_question_ids":["question"],
            "events":[{"kind":"shell", "job_id":"job"}, {"kind":"lane", "notification_id":"notice"}],
            "shell_jobs":[{"job_id":"job", "call_id":"original-shell", "content":"shell body"}]
        });
        let mut waited = super::super::serialize(&snapshot);
        waited.result.progress = Some(tools::Progress::Advanced);
        waited
            .observations
            .messages
            .push(crate::mailbox::QueuedMessage::explicit(
                serde_json::from_value(snapshot["messages"][0].clone()).unwrap(),
            ));
        waited.observations.questions.push("question".into());
        waited.observations.events.push("lane:notice".into());
        waited
            .observations
            .child_progress
            .push(crate::meta::observations::ChildProgress::new(
                "child",
                Some(7),
            ));
        let projected = project_direct_wait_result(waited);
        let value: serde_json::Value = serde_json::from_str(&projected.result.content).unwrap();
        assert_eq!(value["output"], "child body");
        assert_eq!(value["turn_generation"], 7);
        for field in [
            "inbox_ready",
            "messages",
            "user_input_ready",
            "answer_ready",
            "question_ids",
            "attention_question_ids",
            "events",
            "shell_jobs",
        ] {
            assert_eq!(
                value[field], snapshot[field],
                "lost global payload: {field}"
            );
        }
        assert_eq!(projected.result.progress, Some(tools::Progress::Advanced));
        assert_eq!(projected.observations.messages.len(), 1);
        assert_eq!(
            projected.observations.messages[0].message.message_id,
            "message"
        );
        assert_eq!(projected.observations.questions, ["question"]);
        assert_eq!(projected.observations.events, ["lane:notice"]);
        assert_eq!(projected.observations.child_progress[0].run_id, "child");
    }
}
