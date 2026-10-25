//! Narrow the already-authorized tool set; never grant an editing capability.

use config::EditToolConfig;
use providers::ToolSpec;

pub(crate) fn narrow(tools: &[ToolSpec], choice: EditToolConfig) -> Vec<ToolSpec> {
    tools
        .iter()
        .filter(|tool| match choice {
            EditToolConfig::ApplyPatch => !matches!(tool.name.as_str(), "edit" | "write"),
            EditToolConfig::Edit => tool.name != "apply_patch",
        })
        .cloned()
        .collect()
}

/// Legacy/fixed models do not know about per-candidate selection. Keep this
/// fallback at the model-call boundary, not in the authorized/checkpoint tools.
pub(crate) fn for_model<'a>(
    model: &dyn crate::AgentModel,
    tools: &'a [ToolSpec],
) -> std::borrow::Cow<'a, [ToolSpec]> {
    if model.narrows_edit_tools() {
        tools.into()
    } else {
        narrow(tools, EditToolConfig::Edit).into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrowing_preserves_order_schemas_and_only_removes_tools() {
        let tools = ["shell", "write", "read", "apply_patch", "edit"]
            .into_iter()
            .map(|name| ToolSpec {
                name: name.into(),
                description: format!("schema of {name}"),
                input_schema: serde_json::json!({"const": name}),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            narrow(&tools, EditToolConfig::ApplyPatch),
            vec![tools[0].clone(), tools[2].clone(), tools[3].clone()]
        );
        assert_eq!(
            narrow(&tools, EditToolConfig::Edit),
            vec![
                tools[0].clone(),
                tools[1].clone(),
                tools[2].clone(),
                tools[4].clone()
            ]
        );
        for choice in EditToolConfig::ALL {
            assert!(narrow(&[], choice).is_empty());
            assert_eq!(narrow(&tools[..1], choice), tools[..1]);
        }
        assert!(narrow(&tools[1..2], EditToolConfig::ApplyPatch).is_empty());
        assert!(narrow(&tools[3..4], EditToolConfig::Edit).is_empty());
    }
}
