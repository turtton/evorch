//! Worker A: atomic 0600 TOML-only editing with source revision checks.
use super::{McpConfigError, McpOverrideMutation, ProjectMcpConfig, SourceRevision};
use std::path::Path;
pub fn save_override(
    _root: &Path,
    _expected_revision: SourceRevision,
    _mutation: McpOverrideMutation,
) -> Result<ProjectMcpConfig, McpConfigError> {
    Err(McpConfigError::Unavailable)
}
