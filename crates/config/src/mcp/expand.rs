//! Worker A: shared non-recursive template grammar for JSON and TOML.
use super::{McpConfigError, Template};
pub fn parse_template(_input: &str) -> Result<Template, McpConfigError> {
    Err(McpConfigError::Unavailable)
}
/// Explicit lookup keeps tests/process environment isolated; never calls set_var.
pub fn expand_template(
    _template: &Template,
    _lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, McpConfigError> {
    Err(McpConfigError::Unavailable)
}
