use super::LoopState;
use crate::ownership::Lease;
use providers::ToolSpec;
use serde::Deserialize;
use tools::ToolResult;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimArgs {
    task_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteArgs {
    task_id: String,
    generation: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FindingArgs {
    task_id: String,
    content: String,
    evidence: String,
}

impl LoopState {
    pub(super) fn team_worker(&self) -> bool {
        self.task.role == agents::Role::Worker
            && self.task.config.topology.worker_limit().is_some()
            && self.task.config.team.is_some()
    }

    pub(super) fn add_team_tools(&mut self) {
        if self.task.role == agents::Role::Orchestrator && self.task.config.team.is_some() {
            for spec in &mut self.tool_specs {
                if matches!(spec.name.as_str(), "delegate" | "delegate_background") {
                    spec.description = "Delegate a task. In team mode use role=worker and task={id: unique task id, paths: relative artifact paths}. At most three workers may run concurrently. Workers must claim the task before editing and complete it afterward. No automatic merge.".into();
                }
            }
        }
        if self.team_worker() {
            for (name, schema) in [
                (
                    "task_claim",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"}},"required":["task_id"],"additionalProperties":false}),
                ),
                (
                    "task_complete",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"generation":{"type":"integer"}},"required":["task_id","generation"],"additionalProperties":false}),
                ),
                (
                    "finding_append",
                    serde_json::json!({"type":"object","properties":{"task_id":{"type":"string"},"content":{"type":"string"},"evidence":{"type":"string"}},"required":["task_id","content","evidence"],"additionalProperties":false}),
                ),
            ] {
                self.tool_specs.push(ToolSpec {
                    name: name.into(),
                    description: name.into(),
                    input_schema: schema,
                });
            }
        }
    }

    pub(super) async fn team_tool(&self, name: &str, input: serde_json::Value) -> ToolResult {
        match self.dispatch_team_tool(name, input).await {
            Ok(text) => ToolResult::success(text),
            Err(error) => ToolResult::error(error),
        }
    }

    async fn dispatch_team_tool(
        &self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<String, String> {
        if !self.team_worker() {
            return Err("team overlay is disabled".into());
        }
        let team = self.task.config.team.as_ref().ok_or("team is missing")?;
        let worker = self.task.run_id.to_string();
        match name {
            "task_claim" => {
                let args: ClaimArgs = serde_json::from_value(input).map_err(|e| e.to_string())?;
                let lease = team
                    .board
                    .claim(&args.task_id, &worker, team.now())
                    .map_err(|e| e.to_string())?;
                serde_json::to_string(&lease).map_err(|e| e.to_string())
            }
            "task_complete" => {
                let args: CompleteArgs =
                    serde_json::from_value(input).map_err(|e| e.to_string())?;
                let token = Lease {
                    owner_id: worker,
                    generation: args.generation,
                    expires_at: 0,
                };
                team.board
                    .complete(&args.task_id, &token, team.now())
                    .map_err(|e| e.to_string())?;
                Ok("completed".into())
            }
            "finding_append" => {
                let args: FindingArgs = serde_json::from_value(input).map_err(|e| e.to_string())?;
                let owns_task = team.board.snapshot().map_err(|e| e.to_string())?.iter().any(|task| {
                    task.spec.id == args.task_id && matches!(&task.state,
                        crate::team::ClaimState::Claimed(lease) if lease.owner_id == worker && lease.expires_at > team.now())
                });
                if !owns_task {
                    return Err("finding requires a live task claim".into());
                }
                let path = team
                    .finding_store
                    .clone()
                    .ok_or("finding storage is not configured")?;
                let finding = storage::memory::Lesson {
                    id: format!("{}:{}", worker, args.task_id),
                    project: team.coordinator.to_string(),
                    task_id: args.task_id,
                    content: args.content,
                    evidence: args.evidence,
                };
                tokio::task::spawn_blocking(move || {
                    let db = storage::Database::open(&storage::StorageConfig {
                        db_path: path,
                        ..Default::default()
                    })?;
                    db.append_finding(&finding)
                })
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?;
                Ok("appended".into())
            }
            _ => Err("unknown team tool".into()),
        }
    }

    pub(super) fn guard_team_artifact(
        &self,
        name: &str,
        input: &serde_json::Value,
    ) -> Result<(), String> {
        if !self.team_worker() {
            return Ok(());
        }
        let team = self.task.config.team.as_ref().ok_or("team is missing")?;
        match name {
            "shell" => {
                Err("team workers use owned edit paths, not unrestricted shell writes".into())
            }
            "edit" => {
                let path = input
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .ok_or("edit path missing")?;
                team.board
                    .authorize_path(
                        &self.task.run_id.to_string(),
                        std::path::Path::new(path),
                        team.now(),
                    )
                    .map_err(|e| e.to_string())
            }
            _ => Ok(()),
        }
    }
}
