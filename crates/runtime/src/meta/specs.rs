use providers::ToolSpec;

pub(crate) fn tool_spec(name: &str) -> ToolSpec {
    match name {
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
