//! Shell-only authorization claims. Other review kinds never consult them.
use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorizationBasis {
    ExplicitPermission,
    OrdinaryTask,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeMatch {
    Full,
    Partial,
    Unknown,
}

pub(super) fn shell_verdict_schema() -> providers::JsonSchema {
    let mut schema = verdict_schema();
    let properties = schema.schema["properties"].as_object_mut().unwrap();
    properties.insert(
        "authorization_basis".into(),
        serde_json::json!({
            "type":"string", "enum":["explicit_permission", "ordinary_task", "none"]
        }),
    );
    properties.insert(
        "authorization_refs".into(),
        serde_json::json!({
            "type":"array", "items":{"type":"integer", "minimum":0}
        }),
    );
    properties.insert(
        "scope_match".into(),
        serde_json::json!({
            "type":"string", "enum":["full", "partial", "unknown"]
        }),
    );
    for field in ["authorization_basis", "authorization_refs", "scope_match"] {
        schema.schema["required"]
            .as_array_mut()
            .unwrap()
            .push(field.into());
    }
    schema
}

/// The indices refer to the exact lineage-filtered payload list, not storage IDs.
/// Existence is necessary, but does not prove semantic authorization.
pub(super) fn judge_shell(
    verdict: &WireVerdict,
    procedure_source: bool,
    user_request_count: usize,
) -> ReviewVerdict {
    if !verdict.approve {
        return judge(verdict, "shell action", false);
    }
    if user_request_count == 0 {
        return ReviewVerdict::Deny {
            reason: "shell action lacks a real user request in this run's lineage".into(),
        };
    }
    if verdict
        .authorization_refs
        .as_ref()
        .is_some_and(|refs| refs.iter().any(|&index| index >= user_request_count))
    {
        return ReviewVerdict::Deny {
            reason: "shell action cites an invalid real-user authorization reference".into(),
        };
    }
    if verdict.reason.is_none()
        || verdict.risk_level.is_none()
        || verdict.authorization_level.is_none()
        || verdict.procedure_step.is_none()
        || verdict.authorization_basis.is_none()
        || verdict.authorization_refs.is_none()
        || verdict.scope_match.is_none()
    {
        return ReviewVerdict::RequiresUser {
            reason: "shell authorization fields are incomplete".into(),
        };
    }
    match verdict.scope_match {
        Some(ScopeMatch::Partial) => {
            return ReviewVerdict::Deny {
                reason: "shell action includes targets or effects outside the authorized scope"
                    .into(),
            };
        }
        Some(ScopeMatch::Unknown) => {
            return ReviewVerdict::RequiresUser {
                reason: "shell action's complete authorization scope is uncertain".into(),
            };
        }
        _ => {}
    }
    // Keep the existing verified-procedure route as an OR, including medium authorization.
    let existing = judge(verdict, "shell action", procedure_source);
    if matches!(existing, ReviewVerdict::RequiresUser { .. })
        && verdict.risk_level == Some(RiskLevel::Critical)
        && verdict.authorization_level == Some(AuthorizationLevel::High)
        && verdict.authorization_basis == Some(AuthorizationBasis::ExplicitPermission)
        && verdict.scope_match == Some(ScopeMatch::Full)
        && verdict
            .authorization_refs
            .as_ref()
            .is_some_and(|refs| !refs.is_empty())
    {
        return ReviewVerdict::Approve;
    }
    existing
}
