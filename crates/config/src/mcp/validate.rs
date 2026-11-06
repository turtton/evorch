//! Worker A: validate without executing, resolving DNS or reading keychains.
use super::{McpConfigError, McpServerDefinition};
pub fn validate_server(
    _server_id: &str,
    _definition: &McpServerDefinition,
) -> Result<(), McpConfigError> {
    Err(McpConfigError::Unavailable)
}
