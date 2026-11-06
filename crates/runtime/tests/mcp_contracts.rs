use agents::Role;
use runtime::mcp::*;
use sandbox::{CredentialError, CredentialStore, Secret};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use tools::mcp::{
    McpErrorKind,
    transport::{CancelContext, McpProcessLimiter},
    types::*,
};
struct SpyStore(AtomicUsize);
impl CredentialStore for SpyStore {
    fn get(&self, _: &str) -> Result<Option<Secret>, CredentialError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(None)
    }
    fn set(&self, _: &str, _: &Secret) -> Result<(), CredentialError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    fn delete(&self, _: &str) -> Result<(), CredentialError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
#[tokio::test]
async fn mcp_scaffold_is_inert_with_injected_store() {
    let store = Arc::new(SpyStore(AtomicUsize::new(0)));
    let service = UnavailableMcpService::new(McpCredentialBackend::Available(store.clone()));
    let root = PathBuf::from("/nonexistent/mcp-project");
    assert!(service.subscribe_status(root.clone()).borrow().is_empty());
    assert_eq!(
        service.settings(root.clone()).await.unwrap_err(),
        McpErrorKind::Unavailable
    );
    let scope = ApprovalScope {
        canonical_root: root.clone(),
        server_id: "fixture".into(),
    };
    let revision = config::mcp::SourceRevision::from_sources(None, None);
    assert_eq!(
        service
            .test_connection(scope, revision, CancelContext::default())
            .await
            .unwrap_err(),
        McpErrorKind::NeedsApproval
    );
    assert!(matches!(
        service
            .prepare_new_run(PrepareMcpRun {
                canonical_root: root,
                run_id: "fixture".into(),
                role: Role::Worker,
                cancel: CancelContext::default()
            })
            .await,
        Err(McpErrorKind::Unavailable)
    ));
    let lease = service
        .restore_snapshot(RestoreMcpRun {
            context: PrepareMcpRun {
                canonical_root: PathBuf::from("/canonical/project"),
                run_id: "restored".into(),
                role: Role::Worker,
                cancel: CancelContext::default(),
            },
            snapshot: FrozenMcpSnapshot::default(),
        })
        .unwrap();
    assert!(lease.snapshot().tools.is_empty());
    assert_eq!(store.0.load(Ordering::SeqCst), 0);
}
#[test]
fn mcp_project_limiter_shares_admission_and_releases_on_drop() {
    let limiter = Arc::new(ProjectMcpLimiter::default());
    let root = PathBuf::from("/canonical/project");
    let mut permits: Vec<_> = (0..MAX_STDIO_PER_PROJECT)
        .map(|_| limiter.try_acquire(&root).unwrap())
        .collect();
    assert!(matches!(
        limiter.clone().try_acquire(&root),
        Err(McpErrorKind::ProcessLimitReached)
    ));
    let other = limiter
        .try_acquire(&PathBuf::from("/other/project"))
        .unwrap();
    drop(permits.pop());
    let replacement = limiter.try_acquire(&root).unwrap();
    drop((permits, replacement, other));
    assert!(limiter.try_acquire(&root).is_ok());
}
#[test]
fn mcp_restore_snapshot_rejects_unknown_fields() {
    assert!(
        serde_json::from_value::<FrozenMcpSnapshot>(
            serde_json::json!({"tools":[],"session_id":"forbidden"})
        )
        .is_err()
    );
}
#[test]
fn mcp_config_role_keys_match_agents() {
    let roles = [
        Role::Orchestrator,
        Role::Explorer,
        Role::Worker,
        Role::Reviewer,
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
        Role::MultimodalLooker,
        Role::Evaluator,
    ];
    assert_eq!(
        roles.map(|role| role.config_key()).as_slice(),
        config::mcp::MCP_ROLE_KEYS
    );
}
