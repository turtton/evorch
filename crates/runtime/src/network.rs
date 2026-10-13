//! Process sandboxes keep network isolation; reviewed shell calls select a
//! network-enabled variant for one invocation. Web tools use a separate
//! tool-call authorization gate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sandbox::{BwrapConfig, BwrapSandbox, Sandbox, SandboxBoundary, SandboxError};

use crate::policy::ExecutionPolicy;
use crate::runtime::{IsolatedMounts, SandboxFactory};
use crate::workspace::OwnedWorktree;

/// Build the default process sandbox with networking isolated. A reviewed
/// shell invocation may request a network-only variant through `Sandbox`.
pub fn build_sandbox(
    _policy: &ExecutionPolicy,
    workspace_root: PathBuf,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    let boundary = sandbox_boundary(&IsolatedMounts::shared(workspace_root))?;
    detect(boundary)
}

/// The one place a run's boundary is assembled. Production bwrap mounts and
/// the direct file-tool check are both built from its result (ADR 0033).
pub(crate) fn sandbox_boundary(mounts: &IsolatedMounts) -> Result<SandboxBoundary, SandboxError> {
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
    Ok(boundary)
}

fn detect(boundary: SandboxBoundary) -> Result<Arc<dyn Sandbox>, SandboxError> {
    BwrapSandbox::detect(BwrapConfig::from_boundary(boundary).allow_network(false))
        .map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
}

/// isolated worktree が git 操作に必要とする最小 mount set を構築する。
///
/// `packed-refs` の rewrite (`git pack-refs` / auto gc) は意図的に writable にしない。
/// 通常の branch 更新に必要な個別 metadata だけを writable にする最小性との trade-off。
pub(crate) fn isolated_mounts(worktree: &OwnedWorktree, git_common_dir: &Path) -> IsolatedMounts {
    let worktree_name = match worktree.path.file_name() {
        Some(name) => name,
        None => worktree.path.as_os_str(),
    };
    IsolatedMounts {
        workspace_root: worktree.path.clone(),
        ro_binds: vec![git_common_dir.to_path_buf()],
        rw_binds: vec![
            git_common_dir.join("worktrees").join(worktree_name),
            git_common_dir.join("objects"),
            git_common_dir.join("refs/heads"),
            git_common_dir.join("logs"),
        ],
        scratch: None,
    }
}

pub(crate) struct BwrapFactory;

impl SandboxFactory for BwrapFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        detect(sandbox_boundary(mounts)?)
    }
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
        sandbox::PolicyDecision::AutoAllow if tool == "web_fetch" => NetworkAccessDecision::Ask {
            reason: "web_fetch URL requires review".into(),
        },
        sandbox::PolicyDecision::AutoAllow => NetworkAccessDecision::Allow,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agents::Role;
    use sandbox::PolicyDecision;

    // Given: a run's mounts / When: assembling its boundary / Then: the output store,
    // extra binds, scratch and project root are all listed for bwrap and file tools
    #[test]
    fn run_boundary_lists_every_mount_with_its_access() {
        let mounts = IsolatedMounts {
            workspace_root: PathBuf::from("/project"),
            ro_binds: vec![PathBuf::from("/repo/.git")],
            rw_binds: vec![PathBuf::from("/repo/.git/objects")],
            scratch: Some(PathBuf::from("/var/tmp/evorch-scratch-test/t-1")),
        };
        let boundary = sandbox_boundary(&mounts).unwrap();
        let regions = boundary.regions();
        let outputs = tools::output::output_root().unwrap();
        for (path, access) in [
            (outputs.as_path(), sandbox::PathAccess::ReadOnly),
            (Path::new("/repo/.git"), sandbox::PathAccess::ReadOnly),
            (Path::new("/usr"), sandbox::PathAccess::ReadOnly),
            (
                Path::new("/repo/.git/objects"),
                sandbox::PathAccess::ReadWrite,
            ),
            (
                Path::new("/var/tmp/evorch-scratch-test/t-1"),
                sandbox::PathAccess::ReadWrite,
            ),
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
    fn web_search_can_run_but_fetch_requires_call_review() {
        let role = Role::WebResearcher.capabilities();
        assert_eq!(
            judge_web_network_access(
                &role,
                "WebResearcher",
                "web_search",
                PolicyDecision::AutoAllow,
                true
            ),
            NetworkAccessDecision::Allow,
        );
        assert!(matches!(
            judge_web_network_access(
                &role,
                "WebResearcher",
                "web_fetch",
                PolicyDecision::AutoAllow,
                true
            ),
            NetworkAccessDecision::Ask { .. },
        ));
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
