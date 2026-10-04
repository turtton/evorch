use super::*;
use crate::benchmark::{BenchmarkCheckpoint, unsupported, validate_config};

impl LoopState {
    fn benchmark_selected_leaf(&self) -> bool {
        self.benchmark_replay
            || self.shared.runtime.upgrade().is_some_and(|runtime| {
                runtime.benchmark_recording.get().is_some_and(|recording| {
                    self.task.parent.is_some()
                        && recording.selector.role == self.task.role
                        && recording.selector.category == self.task.config.category
                        && *recording
                            .seen
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            + 1
                            == recording.selector.occurrence
                })
            })
    }

    pub(super) fn filter_benchmark_tools(&mut self) {
        let leaf = self.benchmark_selected_leaf();
        self.tool_specs.retain(|tool| {
            tool.name != "git_diff"
                && !thread_goal_tool(&tool.name)
                && (!leaf || !crate::is_meta_op(&tool.name) || tool.name == "finish")
        });
        for tool in &mut self.tool_specs {
            if tool.name == "shell" {
                let mut properties = tool.input_schema["properties"]
                    .as_object()
                    .cloned()
                    .unwrap_or_default();
                properties.retain(|name, _| {
                    matches!(name.as_str(), "command" | "args" | "cwd" | "timeout_ms")
                });
                tool.input_schema = serde_json::json!({
                    "type": "object", "properties": properties,
                    "required": ["command"], "additionalProperties": false,
                });
                tool.description = "Run one synchronous, noninteractive POSIX shell command in the isolated benchmark workspace. Wait for its completion. Use this as the only tool call in its batch. Network access, host access, background jobs and interactive sessions are unavailable.".into();
            } else if tool.name == "delegate" {
                if let Some(properties) = tool.input_schema["properties"].as_object_mut() {
                    for name in ["background", "interactive", "workspace_branch", "task"] {
                        properties.remove(name);
                    }
                    if let Some(workspace) = properties.get_mut("workspace_mode") {
                        workspace["enum"] = serde_json::json!(["shared"]);
                    }
                }
                tool.description = "Delegate a self-contained task to one child using target.role and optional target.category. Use this as the only tool call in its batch and wait for completion. Include purpose, scope, constraints, expected output and validation in the prompt. Children share the benchmark workspace; background, interactive, team and isolated-workspace delegation are unavailable.".into();
            }
        }
    }

    pub(super) fn append_benchmark_instructions(&mut self) {
        let tools = self
            .tool_specs
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let leaf = if self.benchmark_selected_leaf() {
            " This is a self-contained child evaluation: complete the given task with these tools and return the local result. Parent communication, questions, further delegation, skill loading and context compaction are unavailable."
        } else {
            " Delegate sequentially with one awaited child at a time."
        };
        self.context.push_system(&format!(
            "Benchmark execution constraints: available tools are {tools}. This list and the supplied tool schemas govern this run, including when other instructions describe unavailable tools. Process tools must be the only call in their batch. Shell commands must be synchronous and noninteractive inside the isolated workspace; networking, host access and asynchronous jobs are unavailable.{leaf}"
        ));
    }

    pub(super) async fn capture_benchmark(
        &mut self,
        invocation: &AgentInvocationContext,
        messages: &[providers::Message],
    ) -> Result<(), crate::RuntimeError> {
        if self.benchmark_checked {
            return Ok(());
        }
        self.benchmark_checked = true;
        let Some(shared) = self.shared.runtime.upgrade() else {
            return Ok(());
        };
        let Some(recording) = shared.benchmark_recording.get() else {
            return Ok(());
        };
        if self.task.parent.is_none()
            || recording.selector.role != self.task.role
            || recording.selector.category != self.task.config.category
        {
            return Ok(());
        }
        {
            let mut seen = recording
                .seen
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *seen += 1;
            if *seen != recording.selector.occurrence {
                return Ok(());
            }
        }
        validate_config(&self.task.config, self.task.role)?;
        let parent = self.task.parent.expect("selected child");
        crate::AgentRuntime {
            shared: Arc::clone(&shared),
        }
        .validate_benchmark_capture(self.task.run_id, parent)?;
        if !self.context.checkpoints().is_empty() {
            return Err(unsupported("initial input already required compaction"));
        }
        let workspace_root = shared
            .workspaces
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&self.task.run_id)
            .and_then(|workspace| workspace.active_root.clone())
            .ok_or_else(|| unsupported("capture requires a resolved workspace"))?
            .canonicalize()
            .map_err(unsupported)?;
        let model =
            self.shared
                .model
                .benchmark_settings(invocation, self.task.role, &self.tool_specs)?;
        let frozen = Arc::clone(&self.shared.model).freeze_for_benchmark(model.clone())?;
        let checkpoint = BenchmarkCheckpoint {
            version: 1,
            origin_run_id: self.task.run_id,
            parent_run_id: parent,
            role: self.task.role,
            category: self.task.config.category.clone(),
            prompt: self.task.prompt.clone(),
            messages: messages.to_vec(),
            tools: self.tool_specs.clone(),
            workspace_root,
            model,
            selected_model: self
                .shared
                .model
                .selected_model(self.task.role, self.task.config.category.as_deref()),
            budget: self.task.config.budget.clone(),
            load_skills: self.task.config.load_skills.clone(),
        };
        recording
            .recorder
            .capture(&checkpoint)
            .await
            .map_err(unsupported)?;
        self.shared.model = frozen;
        self.shared.compaction.enabled = false;
        self.benchmark = Some(checkpoint);
        Ok(())
    }

    pub(super) fn benchmark_tools_supported(
        &self,
        calls: &[(String, String, serde_json::Value)],
    ) -> Result<(), crate::RuntimeError> {
        let recording = self
            .shared
            .runtime
            .upgrade()
            .is_some_and(|runtime| runtime.benchmark_recording.get().is_some());
        if self.benchmark_replay || recording {
            for (_, name, input) in calls {
                if thread_goal_tool(name) {
                    return Err(unsupported("thread-goal tools require live goal state"));
                }
                if name == "git_diff" {
                    return Err(unsupported(
                        "git_diff can execute repository-configured helpers; benchmark fixtures must be git-free",
                    ));
                }
                if calls.len() != 1
                    && self
                        .shared
                        .executor
                        .tool_permissions(name)
                        .is_some_and(|permissions| permissions.process_spawn)
                {
                    return Err(unsupported(
                        "benchmark process tools must run in a single-tool batch",
                    ));
                }
                if name == "shell"
                    && (calls.len() != 1
                        || input.get("yield_ms").is_some_and(|value| !value.is_null())
                        || input
                            .get("interactive")
                            .and_then(serde_json::Value::as_bool)
                            == Some(true))
                {
                    return Err(unsupported(
                        "benchmark requires a single synchronous, noninteractive shell call without yield_ms",
                    ));
                }
            }
        }
        if self.benchmark.is_some() {
            // Independent replay cannot reconstruct live parent state or user answers.
            // Enforce the restricted interface even if the model invents a hidden tool.
            for (_, name, _) in calls {
                if crate::is_meta_op(name) && name != "finish" {
                    return Err(unsupported(format!(
                        "selected leaf requested side-channel tool `{name}`"
                    )));
                }
            }
        }
        if recording {
            let delegates: Vec<_> = calls
                .iter()
                .filter(|(_, name, _)| name == "delegate")
                .collect();
            if !delegates.is_empty()
                && (calls.len() != 1
                    || delegates.iter().any(|(_, _, input)| {
                        input
                            .get("background")
                            .and_then(serde_json::Value::as_bool)
                            .unwrap_or(false)
                    }))
            {
                return Err(unsupported(
                    "recording requires single awaited delegation, without parallel tool calls",
                ));
            }
        }
        Ok(())
    }
}

fn thread_goal_tool(name: &str) -> bool {
    matches!(
        name,
        "create_goal" | "get_goal" | "submit_goal_check" | "submit_goal_review"
    )
}
