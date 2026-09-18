use std::path::Path;

use crate::{ConfigError, SandboxConfig};

/// Saves only the sandbox section, retaining other settings and comments.
///
/// # Errors
/// Returns configuration validation or filesystem errors without accepting invalid settings.
pub fn save_sandbox(path: &Path, sandbox: SandboxConfig) -> Result<(), ConfigError> {
    let mut doc = crate::save::read_document(path)?;
    let mut section = toml_edit::Table::new();
    section.insert("allow_network", toml_edit::value(sandbox.allow_network));
    doc.insert("sandbox", toml_edit::Item::Table(section));
    crate::save::write_document(path, &doc)
}
