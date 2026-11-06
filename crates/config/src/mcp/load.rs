//! Worker A: bounded parsing and full-definition precedence; no MCP I/O.
use super::{McpConfigError, ProjectMcpConfig};
use std::path::Path;
/// Fail closed until the loader lands; must not make an empty successful config.
pub fn load_project(_root: &Path) -> Result<ProjectMcpConfig, McpConfigError> {
    Err(McpConfigError::Unavailable)
}
