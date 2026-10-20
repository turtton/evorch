//! `sandbox_access` / `justification` arguments shared by read, grep, write and edit.
//!
//! `"isolated"` means "inside the sandbox boundary", the same paths the
//! sandboxed shell sees. It is unrelated to the isolated workspaces removed by
//! ADR 0031; that removal must not touch this value.

pub const SANDBOX_ACCESS: &str = "sandbox_access";
pub const JUSTIFICATION: &str = "justification";

const ACCESS_DESCRIPTION: &str = "isolated (default): only paths the sandboxed shell also sees, meaning the project root and this thread's $TMPDIR (read-write), the allowed directories listed in the system prompt (with their stated access), plus system, Nix store, tool output and Cargo/rustup cache paths (read-only). isolated names this sandbox boundary, not an isolated workspace. unsandboxed: one reviewed access outside that boundary; requires a nonempty justification and may be denied. Paths inside the boundary never need it.";
const JUSTIFICATION_DESCRIPTION: &str = "Why this call needs a path outside the sandbox boundary. Required and nonempty for unsandboxed.";

pub(crate) fn with_access_arguments(mut schema: serde_json::Value) -> serde_json::Value {
    if let Some(properties) = schema["properties"].as_object_mut() {
        properties.insert(
            SANDBOX_ACCESS.into(),
            serde_json::json!({"type":"string", "enum":["isolated", "unsandboxed"], "default":"isolated", "description":ACCESS_DESCRIPTION}),
        );
        properties.insert(
            JUSTIFICATION.into(),
            serde_json::json!({"type":"string", "description":JUSTIFICATION_DESCRIPTION}),
        );
    }
    schema
}
