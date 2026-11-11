//! Production wire ingress over real runtime/SQLite/ownership, including restart.
mod common;
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use serde_json::json;
use std::sync::Arc;
use workbench_host::{CommandHost, commands::CommandService};
use workbench_protocol::{
    commands::{Command, CommandDisposition},
    envelope::Provenance,
    errors::ErrorCode,
    lane_management::*,
    queries::{Query, QueryReply},
    wire::Revision,
};

struct Fixture {
    dir: tempfile::TempDir,
    storage: storage::Storage,
    runtime: AgentRuntime,
    owner: Arc<runtime::ownership::OwnerHost>,
    host: CommandHost,
    lead: RunId,
    lane: runtime::lanes::LaneOpened,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("host.db"),
            ..Default::default()
        };
        let storage = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(256));
        let model = Arc::new(common::HeldModel::default());
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus.clone())),
            model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&config, storage.handle()).unwrap());
        let owner = Arc::new(
            runtime::ownership::OwnerHost::open(
                &dir.path().join("owners"),
                Default::default(),
                bus,
            )
            .unwrap(),
        );
        let permit = owner.start("lead").unwrap();
        let lead = runtime
            .delegate_chat(
                "lead",
                Role::Orchestrator,
                "manage lanes".into(),
                RunConfig {
                    conversation: true,
                    interactive: true,
                    keep_alive: true,
                    ownership: Some(permit),
                    project_root: Some(dir.path().into()),
                    ..Default::default()
                },
            )
            .unwrap();
        model.reached.notified().await;
        let lane = runtime.open_lane(lead, "wire-open", serde_json::from_value(json!({
            "objective":"independent work", "inputs":[], "deliverables":["patch"],
            "acceptance_criteria":["review"], "constraints":[], "workspace":{}, "report_policy":"attached"
        })).unwrap()).unwrap();
        model.reached.notified().await;
        storage
            .handle()
            .upsert_threads_host(&["lead", &lane.thread_id].map(|id| {
                storage::threads::ThreadUpsert {
                    thread_id: id.into(),
                    project_id: Some("project-1".into()),
                    project_root: Some(dir.path().to_string_lossy().into_owned()),
                    ..Default::default()
                }
            }))
            .unwrap();
        std::fs::write(
            dir.path().join("sidebar.json"),
            r#"{"version":2,"families":[]}"#,
        )
        .unwrap();
        let service = CommandService::new(runtime.clone(), tokio::runtime::Handle::current())
            .with_ownership(owner.clone());
        service.bind_lane_delivery();
        let host =
            CommandHost::new(service).with_lane_sidebar_path(dir.path().join("sidebar.json"));
        Self {
            dir,
            storage,
            runtime,
            owner,
            host,
            lead,
            lane,
        }
    }
    fn command(&self, id: &str, revision: u64, action: LaneManagementAction) -> Command {
        Command::ManageLane(ManageLane {
            request: LaneManagementRequest {
                operation_id: LaneManagementOperationId::new(id).unwrap(),
                lane_id: workbench_protocol::lane_attention::LaneId::new(&self.lane.lane_id)
                    .unwrap(),
                expected_management_revision: Revision(revision),
                action,
            },
            expected_family_revision: None,
        })
    }
    fn state(&self) -> LaneManagementState {
        match self
            .host
            .handle_query(
                &workbench_protocol::ids::SessionId::new("wire").unwrap(),
                &Query::GetLaneManagement {
                    lane_id: workbench_protocol::lane_attention::LaneId::new(&self.lane.lane_id)
                        .unwrap(),
                },
            )
            .unwrap()
        {
            QueryReply::LaneManagement(state) => state,
            other => panic!("{other:?}"),
        }
    }
    fn reattach(&self, id: &str, revision: u64) -> Command {
        let mut command = self.command(
            id,
            revision,
            LaneManagementAction::Reattach {
                lead_thread_id: workbench_protocol::ids::ThreadId::new("lead").unwrap(),
                expected_lead_root_run_id: workbench_protocol::ids::RunId::new(
                    self.lead.to_string(),
                )
                .unwrap(),
            },
        );
        if let Command::ManageLane(input) = &mut command {
            input.expected_family_revision = self.state().family_revision;
        }
        command
    }
    async fn stop(&self) {
        for run in [self.lane.root_run_id, self.lead] {
            self.runtime.cancel(run).unwrap();
            self.runtime.wait(run).await.unwrap();
        }
    }
}
#[tokio::test]
async fn wire_roundtrip_replay_stale_revision_and_restart_use_one_delivery_runtime() {
    let mut f = Fixture::new().await;
    for (rev, policy) in [
        (1, ManagedLanePolicy::Summary),
        (2, ManagedLanePolicy::Attached),
    ] {
        let cmd = f.command(
            &format!("policy-{rev}"),
            rev,
            LaneManagementAction::SetPolicy { policy },
        );
        let reply = f.host.handle_command(cmd.clone(), common::web()).unwrap();
        assert_eq!(
            reply.lane_management.unwrap().management_revision,
            Revision(rev + 1)
        );
        assert_eq!(
            f.host
                .handle_command(cmd, common::web())
                .unwrap()
                .disposition,
            CommandDisposition::AlreadyApplied
        );
    }
    let stale = f.command("stale", 1, LaneManagementAction::Detach);
    assert_eq!(
        f.host
            .handle_command(stale, common::web())
            .unwrap_err()
            .code,
        ErrorCode::StaleRevision
    );
    let detach = f.command("detach", 3, LaneManagementAction::Detach);
    f.host.handle_command(detach, common::web()).unwrap();
    assert_eq!(f.state().policy, LaneManagementPolicy::Detached);
    let attach = f.reattach("attach", 4);
    assert_eq!(
        f.host
            .handle_command(attach.clone(), common::cli())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    let mut stale_family = attach.clone();
    if let Command::ManageLane(i) = &mut stale_family {
        i.expected_family_revision.as_mut().unwrap().pin_revision.0 += 1;
    }
    assert_eq!(
        f.host
            .handle_command(stale_family, common::web())
            .unwrap_err()
            .code,
        ErrorCode::StaleRevision
    );
    let reply = f
        .host
        .handle_command(attach.clone(), common::web())
        .unwrap()
        .lane_management
        .unwrap();
    assert_eq!(reply.policy, LaneManagementPolicy::Summary);
    assert!(
        reply.projection_pending,
        "core must not falsely ACK the GUI file"
    );
    assert_eq!(
        f.host
            .handle_command(attach, common::web())
            .unwrap()
            .disposition,
        CommandDisposition::AlreadyApplied
    );
    f.stop().await;
    // A second incarnation over the same durable store renews the current owner,
    // without fabricating a live lead or waking a root for a management operation.
    let restored = common::runtime_on_store(f.dir.path(), &f.storage);
    let service = CommandService::new(restored.clone(), tokio::runtime::Handle::current())
        .with_ownership(f.owner.clone());
    service.bind_lane_delivery();
    f.host = CommandHost::new(service).with_lane_sidebar_path(f.dir.path().join("sidebar.json"));
    let detach = f.command("after-restart", 5, LaneManagementAction::Detach);
    f.host
        .handle_command(detach.clone(), common::web())
        .unwrap();
    assert_eq!(
        f.host
            .handle_command(detach, common::web())
            .unwrap()
            .disposition,
        CommandDisposition::AlreadyApplied
    );
    let attach = f.reattach("attach-after-restart", 6);
    f.host.handle_command(attach, common::web()).unwrap();
    assert_eq!(f.state().management_revision, Revision(7));
    assert!(
        restored.list_agents().is_empty(),
        "human management never starts a run"
    );
}
#[tokio::test]
async fn foreign_missing_owner_agent_and_unmirrored_pin_are_rejected() {
    let mut f = Fixture::new().await;
    let command = f.command(
        "policy",
        1,
        LaneManagementAction::SetPolicy {
            policy: ManagedLanePolicy::Summary,
        },
    );
    assert_eq!(
        f.host
            .handle_command(
                command.clone(),
                Provenance::agent(workbench_protocol::ids::RunId::new(f.lead.to_string()).unwrap())
            )
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    let foreign = Arc::new(
        runtime::ownership::OwnerHost::open(
            &f.dir.path().join("owners"),
            Default::default(),
            f.runtime.event_bus(),
        )
        .unwrap(),
    );
    let service = CommandService::new(
        common::runtime_on_store(f.dir.path(), &f.storage),
        tokio::runtime::Handle::current(),
    )
    .with_ownership(foreign);
    let mut host = CommandHost::new(service);
    assert_eq!(
        host.handle_command(command.clone(), common::web())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    let mut no_owner = CommandHost::new(CommandService::new(
        f.runtime.clone(),
        tokio::runtime::Handle::current(),
    ));
    assert_eq!(
        no_owner
            .handle_command(command, common::web())
            .unwrap_err()
            .code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(f.state().management_revision, Revision(1));
    let detach = f.command("detach", 1, LaneManagementAction::Detach);
    f.host.handle_command(detach, common::web()).unwrap();
    for field in ["pinned", "archived"] {
        let family = json!({"version":2,"families":[{"key":f.lane.thread_id,"membership_epoch":2,(field):true,"pin_revision":8,"archive_revision":9}]});
        std::fs::write(f.dir.path().join("sidebar.json"), family.to_string()).unwrap();
        let command = f.reattach(field, 2);
        assert_eq!(
            f.host
                .handle_command(command, common::web())
                .unwrap_err()
                .code,
            ErrorCode::Conflict
        );
        assert_eq!(f.state().policy, LaneManagementPolicy::Detached);
    }
    f.stop().await;
}

#[tokio::test]
async fn a_human_close_ends_only_the_ledger_entry_and_an_agent_cannot_reach_it() {
    use event_bus::lane::LaneCloseKind;
    use runtime::lane_close::HumanLaneCloseRequest;
    use workbench_protocol::envelope::{Actor, Provenance, Surface};
    fn request(f: &Fixture, discard: bool) -> HumanLaneCloseRequest {
        let lane = f
            .storage
            .handle()
            .lane_close_context(f.lane.lane_id.clone())
            .unwrap();
        HumanLaneCloseRequest {
            lane_id: f.lane.lane_id.clone(),
            ready_seq: None,
            expected_management_revision: lane.management_revision,
            expected_request_revision: lane.request_revision,
            discard,
            note: "ending this lane".into(),
            surface: "forged".into(),
            client_id: None,
        }
    }
    let mut f = Fixture::new().await;
    let agent = Provenance {
        actor: Actor::Agent,
        surface: Surface::Cli,
        client_id: None,
        agent_run_id: None,
    };
    let as_agent = request(&f, false);
    assert_eq!(
        f.host.close_lane_human(as_agent, &agent).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert!(
        f.storage
            .handle()
            .current_lane_close(f.lane.lane_id.clone())
            .unwrap()
            .is_none()
    );
    let discard = request(&f, true);
    let closed = f.host.close_lane_human(discard, &common::web()).unwrap();
    assert_eq!(closed.kind, LaneCloseKind::Discarded);
    // Nothing else changed: the lane is still attached to its lead and its run is alive.
    assert_eq!(f.state().policy, LaneManagementPolicy::Attached);
    assert!(f.runtime.inspect_agent(f.lane.root_run_id).is_ok());
    f.stop().await;
}
