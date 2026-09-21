use providers::ToolSpec;

pub(crate) fn tool_spec(name: &str) -> ToolSpec {
    match name {
        "delegate" => ToolSpec {
            name: name.into(),
            description: "Delegate a task to a child agent. Role defaults to worker. By default, wait for the child and return its phase; background=true returns immediately with a run_id. interactive=true requires background=true. Category is worker-only; images require multimodal_looker (alias: multimodallooker). Provide a self-contained prompt with task, expected outcome, required tools, must-do, must-not-do and context.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "role": {"type": "string", "default": "worker", "enum": [
                        "orchestrator", "explorer", "worker", "reviewer", "planner", "oracle",
                        "multimodal_looker", "multimodallooker"
                    ]},
                    "prompt": {"type": "string", "description": "Self-contained task instructions and relevant context."},
                    "background": {"type": "boolean", "default": false, "description": "Return the child run_id immediately instead of waiting."},
                    "interactive": {"type": "boolean", "default": false, "description": "Keep the child available for messages; requires background=true."},
                    "name": {"type": "string", "description": "Human-readable child name."},
                    "category": {"type": "string", "enum": super::CATEGORIES, "description": "Worker-only task category for model routing."},
                    "workspace_mode": {"type": "string", "enum": ["shared", "isolated"], "default": "shared"},
                    "workspace_branch": {"type": "string", "description": "Existing branch for an isolated workspace."},
                    "load_skills": {"type": "array", "items": {"type": "string"}, "description": "Registered skills to load into the child."},
                    "task": {
                        "type": "object", "description": "Team-mode task assignment.",
                        "properties": {"id": {"type": "string"}, "paths": {"type": "array", "items": {"type": "string"}}},
                        "required": ["id", "paths"], "additionalProperties": false
                    },
                    "images": {
                        "type": "array", "description": "Image payloads for multimodal_looker only.",
                        "items": {"type": "object", "properties": {
                            "media_type": {"type": "string"}, "data": {"type": "string", "description": "Base64-encoded image data."}
                        }, "required": ["media_type", "data"], "additionalProperties": false}
                    }
                },
                "required": ["prompt"]
            }),
        },
        "run_output" => ToolSpec {
            name: name.into(),
            description: "Retrieve a run's output without waiting or consuming its inbox. Only your direct children or parent are readable, like send_message; self, siblings and unrelated runs are denied. Returns phase and status (still_running, completed, cancelled, failed), output only on completion, and reason on failure/cancellation. Does not restore terminal runs.".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"run_id": {"type": "string"}},
                "required": ["run_id"],
            }),
        },
        _ => ToolSpec {
            name: name.into(),
            description: format!("{name} tool"),
            input_schema: serde_json::json!({"type": "object"}),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delegate_schema_exposes_optional_role_and_execution_flags() {
        // Given: the public delegate definition.
        let spec = tool_spec("delegate");
        // When: inspecting the machine-consumed schema.
        let properties = &spec.input_schema["properties"];
        // Then: prompt alone is required; both accepted multimodal aliases are advertised.
        assert_eq!(spec.input_schema["required"], serde_json::json!(["prompt"]));
        assert_eq!(
            properties["role"]["enum"],
            serde_json::json!([
                "orchestrator",
                "explorer",
                "worker",
                "reviewer",
                "planner",
                "oracle",
                "multimodal_looker",
                "multimodallooker"
            ])
        );
        assert_eq!(properties["role"]["default"], "worker");
        for name in ["background", "interactive"] {
            assert_eq!(properties[name]["type"], "boolean");
            assert_eq!(properties[name]["default"], false);
        }
        for name in [
            "prompt",
            "name",
            "category",
            "workspace_mode",
            "workspace_branch",
        ] {
            assert_eq!(properties[name]["type"], "string");
        }
        assert_eq!(properties["load_skills"]["type"], "array");
        assert_eq!(properties["task"]["type"], "object");
        assert_eq!(properties["images"]["type"], "array");
    }

    #[test]
    fn run_output_exposed_with_required_run_id_when_orchestrator() {
        // Given: the canonical registry and role capability filters.
        let specs = crate::META_OPS.iter().map(|name| tool_spec(name)).collect();
        // When: model-visible definitions are selected for the orchestrator.
        let visible =
            crate::ExecutionPolicy::for_role(agents::Role::Orchestrator).filter_tool_specs(specs);
        // Then: the callable tool includes its machine-consumed input contract.
        let spec = visible
            .iter()
            .find(|spec| spec.name == "run_output")
            .unwrap();
        assert_eq!(spec.input_schema["required"], serde_json::json!(["run_id"]));
        assert_eq!(spec.input_schema["properties"]["run_id"]["type"], "string");
    }
}
