//! Process sandboxes keep network isolation; reviewed shell calls select a
//! network-enabled variant for one invocation. Web tools use a separate
//! tool-call authorization gate.

use std::path::PathBuf;
use std::sync::Arc;

use sandbox::{BwrapConfig, BwrapSandbox, Sandbox, SandboxBoundary, SandboxError};

use crate::policy::ExecutionPolicy;
use crate::runtime::{SandboxFactory, WorkspaceMounts};

/// Build the default process sandbox with networking isolated. A reviewed
/// shell invocation may request a network-only variant through `Sandbox`.
pub fn build_sandbox(
    _policy: &ExecutionPolicy,
    workspace_root: PathBuf,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    let boundary = sandbox_boundary(&WorkspaceMounts::shared(workspace_root))?;
    detect(boundary)
}

/// The one place a run's boundary is assembled. Production bwrap mounts and
/// the direct file-tool check are both built from its result (ADR 0033).
pub(crate) fn sandbox_boundary(mounts: &WorkspaceMounts) -> Result<SandboxBoundary, SandboxError> {
    let outputs = tools::output::output_root().map_err(|error| SandboxError::BwrapUnavailable {
        detail: format!("一時ツール出力ディレクトリを作成できません: {error}"),
    })?;
    let mut boundary = SandboxBoundary::new(mounts.workspace_root.clone()).read_only(outputs);
    for path in &mounts.ro_binds {
        boundary = boundary.read_only(path.clone());
    }
    for path in &mounts.rw_binds {
        boundary = boundary.read_write(path.clone());
    }
    if let Some(path) = &mounts.scratch {
        boundary = boundary.with_scratch(path.clone());
    }
    if let Some(allowed) = &mounts.allowed {
        boundary = boundary.with_allowed(allowed.paths.clone(), allowed.access);
    }
    Ok(boundary)
}

fn detect(boundary: SandboxBoundary) -> Result<Arc<dyn Sandbox>, SandboxError> {
    BwrapSandbox::detect(BwrapConfig::from_boundary(boundary).allow_network(false))
        .map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
}

pub(crate) struct BwrapFactory;

impl SandboxFactory for BwrapFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        mounts: &WorkspaceMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        detect(sandbox_boundary(mounts)?)
    }
}

/// Tools whose URL argument is reviewed on every call before it is loaded.
pub(crate) fn reviews_url(tool: &str) -> bool {
    matches!(tool, "web_fetch" | "browser_navigate")
}

/// Hands local files to a web page; every call is reviewed.
pub(crate) const UPLOAD_TOOL: &str = "browser_upload_file";

/// Tools reviewed on every call before they run.
pub(crate) fn reviews_each_call(tool: &str) -> bool {
    reviews_url(tool) || tool == UPLOAD_TOOL
}

/// Authorization outcome for one network-capable tool invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NetworkAccessDecision {
    Allow,
    Ask { reason: String },
    Deny { reason: String },
}

/// Decide a Web tool call using the role allowlist and per-tool policy.
/// `enabled` is a global deny ceiling, never a grant. Fetch reviews its URL
/// on every call; search may proceed automatically when the policy allows it.
pub fn judge_web_network_access(
    role: &agents::RoleCapabilities,
    role_name: &str,
    tool: &str,
    per_tool: sandbox::PolicyDecision,
    enabled: bool,
) -> NetworkAccessDecision {
    match role.check_tool(role_name, tool) {
        agents::CapabilityDecision::Allowed => {}
        agents::CapabilityDecision::Denied { reason, .. } => {
            return NetworkAccessDecision::Deny { reason };
        }
    }
    if !enabled {
        return NetworkAccessDecision::Deny {
            reason: "Web tools are disabled".into(),
        };
    }
    match per_tool {
        sandbox::PolicyDecision::Deny => NetworkAccessDecision::Deny {
            reason: format!("tool '{tool}' is denied by policy"),
        },
        sandbox::PolicyDecision::Ask => NetworkAccessDecision::Ask {
            reason: format!("tool '{tool}' requires review"),
        },
        sandbox::PolicyDecision::AutoAllow if reviews_url(tool) => NetworkAccessDecision::Ask {
            reason: format!("{tool} URL requires review"),
        },
        sandbox::PolicyDecision::AutoAllow if tool == UPLOAD_TOOL => NetworkAccessDecision::Ask {
            reason: format!("each {tool} call requires review"),
        },
        sandbox::PolicyDecision::AutoAllow => NetworkAccessDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use agents::Role;
    use sandbox::PolicyDecision;

    // Given: a run's mounts / When: assembling its boundary / Then: the output store,
    // extra binds, allowed directories, scratch and project root are all listed for
    // bwrap and file tools
    #[test]
    fn run_boundary_lists_every_mount_with_its_access() {
        let mounts = WorkspaceMounts {
            workspace_root: PathBuf::from("/project"),
            ro_binds: vec![PathBuf::from("/opt/reference")],
            rw_binds: vec![PathBuf::from("/opt/shared")],
            scratch: Some(PathBuf::from("/var/tmp/evorch-scratch-test/t-1")),
            allowed: Some(sandbox::AllowedRegions {
                paths: sandbox::LivePaths::new(vec![PathBuf::from("/srv/allowed")]),
                access: sandbox::PathAccess::ReadOnly,
            }),
        };
        let boundary = sandbox_boundary(&mounts).unwrap();
        let regions = boundary.regions();
        let outputs = tools::output::output_root().unwrap();
        for (path, access) in [
            (outputs.as_path(), sandbox::PathAccess::ReadOnly),
            (Path::new("/opt/reference"), sandbox::PathAccess::ReadOnly),
            (Path::new("/usr"), sandbox::PathAccess::ReadOnly),
            (Path::new("/opt/shared"), sandbox::PathAccess::ReadWrite),
            (
                Path::new("/var/tmp/evorch-scratch-test/t-1"),
                sandbox::PathAccess::ReadWrite,
            ),
            (Path::new("/srv/allowed"), sandbox::PathAccess::ReadOnly),
            (Path::new("/project"), sandbox::PathAccess::ReadWrite),
        ] {
            assert!(
                regions
                    .iter()
                    .any(|region| region.path == path && region.access == access),
                "{}",
                path.display()
            );
        }
        assert_eq!(
            BwrapConfig::from_boundary(boundary).boundary().regions(),
            regions
        );
    }

    #[test]
    fn only_url_loading_and_upload_tools_require_call_review() {
        assert_eq!(agents::BROWSER_TOOLS, tools::BROWSER_TOOL_NAMES);
        let role = Role::WebResearcher.capabilities();
        for tool in ["web_search", "web_fetch"]
            .into_iter()
            .chain(agents::BROWSER_TOOLS)
        {
            let decision = judge_web_network_access(
                &role,
                "WebResearcher",
                tool,
                PolicyDecision::AutoAllow,
                true,
            );
            if matches!(tool, "web_fetch" | "browser_navigate" | UPLOAD_TOOL) {
                assert!(
                    matches!(decision, NetworkAccessDecision::Ask { .. }),
                    "{tool}"
                );
            } else {
                assert_eq!(decision, NetworkAccessDecision::Allow, "{tool}");
            }
        }
    }

    #[test]
    fn role_and_global_ceiling_deny_before_tool_policy() {
        let role = Role::Worker.capabilities();
        assert!(matches!(
            judge_web_network_access(
                &role,
                "Worker",
                "web_search",
                PolicyDecision::AutoAllow,
                true
            ),
            NetworkAccessDecision::Deny { .. },
        ));
        let role = Role::WebResearcher.capabilities();
        assert!(matches!(
            judge_web_network_access(
                &role,
                "WebResearcher",
                "web_search",
                PolicyDecision::AutoAllow,
                false
            ),
            NetworkAccessDecision::Deny { .. },
        ));
        assert!(matches!(
            judge_web_network_access(
                &role,
                "WebResearcher",
                "web_search",
                PolicyDecision::Deny,
                true
            ),
            NetworkAccessDecision::Deny { .. },
        ));
    }
}
