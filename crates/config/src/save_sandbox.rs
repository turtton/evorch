use std::path::Path;

use crate::{ConfigError, SandboxConfig};

/// Saves only the sandbox section, retaining other settings and comments.
///
/// # Errors
/// Returns configuration validation or filesystem errors without accepting invalid settings.
pub fn save_sandbox(path: &Path, sandbox: SandboxConfig) -> Result<(), ConfigError> {
    save_sandbox_in_scope(path, sandbox, crate::save_context::SaveScope::Standalone)
}

pub(crate) fn save_sandbox_in_scope(
    path: &Path,
    sandbox: SandboxConfig,
    scope: crate::save_context::SaveScope<'_>,
) -> Result<(), ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    let mut section = toml_edit::Table::new();
    section.insert(
        "web_tools_enabled",
        toml_edit::value(sandbox.web_tools_enabled),
    );
    let escalation_approval = match sandbox.escalation_approval {
        crate::EscalationApproval::Auto => "auto",
        crate::EscalationApproval::User => "user",
        crate::EscalationApproval::Off => "off",
        crate::EscalationApproval::Yolo => "yolo",
    };
    section.insert("escalation_approval", toml_edit::value(escalation_approval));
    section.insert(
        "escalate_to_user_on_deny",
        toml_edit::value(sandbox.escalate_to_user_on_deny),
    );
    doc.insert("sandbox", toml_edit::Item::Table(section));
    crate::save::write_document_in_scope(path, &doc, scope)
}
