use super::*;
use crate::restart::{
    RestartCoordinator, RestartId, RestartPhase,
    public::{FinalizationOwner, LiveUndisposedAdmission, PublicRestartCoordinator},
};
use workbench_host::admin::{AdminContinuation, AdminFacade};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost, TestSeams};
use workbench_protocol::{
    errors::ErrorCode,
    namespaces::lifecycle::{
        AdminId, LifecycleCommand, LifecycleQuery, LifecycleReply,
        restart::{RequestRestart, RestartBinding, RestartCommand, RestartQuery, RestartReply},
    },
    wire::{InstanceEpoch, Revision},
};

use crate::restart::tests::restart_model as model;

struct NoFinalization;
impl FinalizationOwner for NoFinalization {
    fn finalize(
        &self,
        _: RestartCoordinator,
        _: RestartId,
        _: Arc<CurrentElf>,
        _: core_launcher::handoff::targets::CaptureCheckedTarget,
    ) -> AdminContinuation {
        panic!("inspection/listing must never finalize")
    }
}

#[tokio::test]
async fn only_current_and_local_ids_are_listed_without_paths_or_probes() {
    let dir = test_socket_dir::SplitDir::new();
    let root = dir.path().canonicalize().unwrap();
    // A listing/constructor is not permission to execute even a configured path.
    std::fs::write(
        dir.path().join("config.toml"),
        "[core.restart.targets.local]\npath = '/nonexistent-target'\n",
    )
    .unwrap();
    let mut boot = BootstrappedHost::open_for_core_with_test_seams(
        BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        },
        TestSeams {
            model: Arc::new(model::Model::default()),
            sandbox: Arc::new(model::Direct),
        },
        &crate::recovery_service::CoreGraphRecovery,
    )
    .await
    .unwrap();
    let core = RestartCoordinator::new(&boot);
    let epoch = InstanceEpoch(1);
    let current = Arc::new(CurrentElf::record().unwrap());
    let targets = build(root, current).unwrap();
    let leaf = PublicRestartCoordinator::new(
        core.clone(),
        epoch,
        targets,
        Arc::new(super::super::recipes::HostRecipeSource::new(&boot)),
        Arc::new(LiveUndisposedAdmission {
            collector: boot.graph_collector().clone(),
            core: core.clone(),
            epoch,
        }),
        Arc::new(NoFinalization),
    );
    let (socket, _peer) = tokio::net::UnixStream::pair().unwrap();
    let lease = workbench_authority::verify_unix_peer(&socket).unwrap();
    let verified = lease.authority().connection().unwrap();
    let reply = leaf
        .query(&LifecycleQuery::Restart(RestartQuery::Targets), &verified)
        .await
        .unwrap();
    let LifecycleReply::Restart(RestartReply::Targets(targets)) = reply else {
        panic!("expected local target IDs")
    };
    assert_eq!(
        targets
            .targets
            .iter()
            .map(|t| t.id.as_str())
            .collect::<Vec<_>>(),
        ["current", "local"]
    );
    let wire = serde_json::to_string(&targets).unwrap();
    assert!(!wire.contains("/nonexistent-target"));
    let request = RequestRestart {
        binding: RestartBinding {
            target_id: AdminId::new("not-configured").unwrap(),
            expected_core_epoch: epoch,
            target_revision: targets.revision,
            operation_id: AdminId::new("inspect").unwrap(),
        },
        confirmed: true,
    };
    let intent = LifecycleCommand::Restart(RestartCommand::Request(request.clone()));
    assert_eq!(
        leaf.inspect(&intent, &verified).await.err().unwrap().code,
        ErrorCode::Conflict
    );
    assert_eq!(core.snapshot().phase, RestartPhase::Running);
    assert_eq!(core.snapshot().revision, 0);
    for key in ["path", "argv", "env"] {
        let mut value = serde_json::to_value(&request).unwrap();
        value["binding"][key] = serde_json::json!("/untrusted-input");
        assert!(serde_json::from_value::<RequestRestart>(value).is_err());
    }
    assert_ne!(targets.revision, Revision(0));
    boot.shutdown().await.unwrap();
}
