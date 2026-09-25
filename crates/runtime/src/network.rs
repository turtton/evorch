//! Process sandboxes keep network isolation; reviewed shell calls select a
//! network-enabled variant for one invocation. Web tools use a separate
//! tool-call authorization gate.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use sandbox::{BwrapConfig, BwrapSandbox, Sandbox, SandboxError};

use crate::policy::ExecutionPolicy;
use crate::runtime::{IsolatedMounts, SandboxFactory};
use crate::workspace::OwnedWorktree;

/// Build the default process sandbox with networking isolated. A reviewed
/// shell invocation may request a network-only variant through `Sandbox`.
pub fn build_sandbox(
    _policy: &ExecutionPolicy,
    workspace_root: PathBuf,
) -> Result<Arc<dyn Sandbox>, SandboxError> {
    let config = base_config(workspace_root)?;
    BwrapSandbox::detect(config).map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
}

fn base_config(workspace_root: PathBuf) -> Result<BwrapConfig, SandboxError> {
    let outputs = tools::output::output_root().map_err(|error| SandboxError::BwrapUnavailable {
        detail: format!("一時ツール出力ディレクトリを作成できません: {error}"),
    })?;
    Ok(BwrapConfig::new(workspace_root)
        .allow_network(false)
        .ro_bind(outputs))
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
    }
}

pub(crate) struct BwrapFactory;

impl SandboxFactory for BwrapFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        let mut config = base_config(mounts.workspace_root.clone())?;
        for path in &mounts.ro_binds {
            config = config.ro_bind(path.clone());
        }
        for path in &mounts.rw_binds {
            config = config.rw_bind(path.clone());
        }
        BwrapSandbox::detect(config).map(|detected| Arc::new(detected) as Arc<dyn Sandbox>)
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
