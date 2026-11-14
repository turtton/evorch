//! Production wire ingress over real runtime/SQLite/ownership, including restart.
mod common;
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use serde_json::json;
use std::sync::Arc;
use workbench_host::{CommandHost, commands::CommandService};
use workbench_protocol::namespaces::{LanesCommand, LanesQuery, LanesReply};
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
        let dir = test_socket_dir::tempdir();
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
        let host = CommandHost::new(service)
            .with_lane_sidebar_path(dir.path().join("sidebar.json"))
            .with_areas(workbench_host::areas::Areas {
                lanes: workbench_host::areas::lanes::LanesArea::with_runtime(runtime.clone()),
                ..Default::default()
            });
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
        Command::Lanes(LanesCommand::ManageLane(ManageLane {
            request: LaneManagementRequest {
                operation_id: LaneManagementOperationId::new(id).unwrap(),
                lane_id: workbench_protocol::lane_attention::LaneId::new(&self.lane.lane_id)
                    .unwrap(),
                expected_management_revision: Revision(revision),
                action,
            },
            expected_family_revision: None,
        }))
    }
    fn state(&self) -> LaneManagementState {
        match self
            .host
            .handle_query(
                &workbench_protocol::ids::SessionId::new("wire").unwrap(),
                &Query::Lanes(LanesQuery::GetLaneManagement {
                    lane_id: workbench_protocol::lane_attention::LaneId::new(&self.lane.lane_id)
                        .unwrap(),
                }),
            )
            .unwrap()
        {
            QueryReply::Lanes(LanesReply::LaneManagement(state)) => state,
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
        if let Command::Lanes(LanesCommand::ManageLane(input)) = &mut command {
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
        assert_eq!(lane_result(reply).management_revision, Revision(rev + 1));
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
    if let Command::Lanes(LanesCommand::ManageLane(i)) = &mut stale_family {
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
        .unwrap();
    let reply = lane_result(reply);
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
    let live = |f: &Fixture| {
        (
            f.runtime.inspect_agent(f.lane.root_run_id).unwrap().phase,
            f.runtime.inspect_agent(f.lead).unwrap().phase,
        )
    };
    let before = live(&f);
    let running = |phase| {
        matches!(
            phase,
            event_bus::AgentRunPhase::Pending
                | event_bus::AgentRunPhase::Running
                | event_bus::AgentRunPhase::Waiting
        )
    };
    assert!(running(before.0) && running(before.1), "{before:?}");
    let closed = f.host.close_lane_human(discard, &common::web()).unwrap();
    assert_eq!(closed.kind, LaneCloseKind::Discarded);
    // Nothing else changed: the lane is still attached to its lead, and neither the lane's
    // run nor the lead's was stopped or finished by the close.
    assert_eq!(f.state().policy, LaneManagementPolicy::Attached);
    assert_eq!(live(&f), before);
    f.stop().await;
}

/// The lane management result a command answered with.
fn lane_result(
    reply: workbench_protocol::commands::CommandReply,
) -> workbench_protocol::lane_management::LaneManagementResult {
    match reply.result.map(|result| *result) {
        Some(workbench_protocol::commands::CommandResult::Lanes(
            workbench_protocol::namespaces::LanesResult::Managed(result),
        )) => result,
        other => panic!("expected a lane management result, got {other:?}"),
    }
}

#[tokio::test]
async fn a_reason_over_the_limit_and_a_detached_lane_are_refused_without_a_record() {
    let mut f = Fixture::new().await;
    let lane_id = f.lane.lane_id.clone();
    let seen = f
        .storage
        .handle()
        .lane_close_context(lane_id.clone())
        .unwrap();
    let close = |note: String, management_revision: u64| {
        Command::Lanes(LanesCommand::CloseLane(CloseLane {
            lane_id: workbench_protocol::lane_attention::LaneId::new(&lane_id).unwrap(),
            kind: HumanLaneCloseKind::Complete,
            ready_seq: None,
            expected_management_revision: Revision(management_revision),
            expected_request_revision: Revision(seen.request_revision),
            note,
        }))
    };
    let standing = |f: &Fixture| {
        f.storage
            .handle()
            .current_lane_close(lane_id.clone())
            .unwrap()
    };

    // One byte over the ledger's limit is refused before anything is recorded.
    let too_long = "x".repeat(storage::lane_control::LANE_CLOSE_NOTE_MAX_BYTES + 1);
    let refused = f
        .host
        .handle_command(close(too_long, seen.management_revision), common::web())
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::LimitExceeded);
    assert!(standing(&f).is_none());

    // A detached lane is not managed by its lead, so nobody can end it from here.
    let detach = f.command(
        "detach",
        seen.management_revision,
        LaneManagementAction::Detach,
    );
    f.host.handle_command(detach, common::web()).unwrap();
    assert_eq!(f.state().policy, LaneManagementPolicy::Detached);
    let detached = f.state().management_revision.0;
    assert!(
        f.host
            .handle_command(close("reason".into(), detached), common::web())
            .is_err()
    );
    assert!(standing(&f).is_none());
    f.stop().await;
}

/// The lane close result a command answered with.
fn close_result(
    reply: workbench_protocol::commands::CommandReply,
) -> workbench_protocol::lane_management::LaneCloseResult {
    match reply.result.map(|result| *result) {
        Some(workbench_protocol::commands::CommandResult::Lanes(
            workbench_protocol::namespaces::LanesResult::Closed(result),
        )) => result,
        other => panic!("expected a lane close result, got {other:?}"),
    }
}

#[tokio::test]
async fn a_wire_close_is_recorded_under_the_authenticated_provenance_and_a_stale_screen_is_refused()
{
    use event_bus::lane::{LaneCloseActor, LaneCloseKind};
    use workbench_protocol::envelope::Provenance;
    let mut f = Fixture::new().await;
    let lane_id = f.lane.lane_id.clone();
    let seen = f
        .storage
        .handle()
        .lane_close_context(lane_id.clone())
        .unwrap();
    let close = |kind, note: &str, request_revision: u64| {
        Command::Lanes(LanesCommand::CloseLane(CloseLane {
            lane_id: workbench_protocol::lane_attention::LaneId::new(&lane_id).unwrap(),
            kind,
            ready_seq: None,
            expected_management_revision: Revision(seen.management_revision),
            expected_request_revision: Revision(request_revision),
            note: note.into(),
        }))
    };
    let standing = |f: &Fixture| {
        f.storage
            .handle()
            .current_lane_close(lane_id.clone())
            .unwrap()
    };

    // An agent's provenance cannot end a lane, whatever the request says.
    let agent = Provenance::agent(workbench_protocol::ids::RunId::new("run-agent").unwrap());
    let refused = f
        .host
        .handle_command(
            close(
                HumanLaneCloseKind::Discard,
                "I decided",
                seen.request_revision,
            ),
            agent,
        )
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::PermissionDenied);
    // The reason is required before the ledger is touched.
    for note in ["", "   "] {
        let invalid = f
            .host
            .handle_command(
                close(HumanLaneCloseKind::Discard, note, seen.request_revision),
                common::web(),
            )
            .unwrap_err();
        assert_eq!(invalid.code, ErrorCode::InvalidRequest, "{note:?}");
    }
    // A screen that read an older request revision is refused as stale; nothing is recorded.
    let stale = f
        .host
        .handle_command(
            close(
                HumanLaneCloseKind::Discard,
                "late",
                seen.request_revision + 1,
            ),
            common::web(),
        )
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::StaleRevision);
    assert!(standing(&f).is_none());

    // The record names the surface and client the host authenticated.
    let reply = f
        .host
        .handle_command(
            close(
                HumanLaneCloseKind::Complete,
                "reviewed by hand",
                seen.request_revision,
            ),
            common::web(),
        )
        .unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    let result = close_result(reply);
    assert_eq!(result.kind, LaneCloseRecordKind::HumanClosed);
    assert!(!result.already_closed);
    let stored = standing(&f).unwrap();
    assert_eq!(stored.kind, LaneCloseKind::HumanClosed);
    assert_eq!(
        stored.closed_by,
        Some(LaneCloseActor::Human {
            surface: "web".into(),
            client_id: Some("web-1".into()),
        })
    );

    // The same request again changes nothing and says so.
    let again = f
        .host
        .handle_command(
            close(
                HumanLaneCloseKind::Complete,
                "reviewed by hand",
                seen.request_revision,
            ),
            common::web(),
        )
        .unwrap();
    assert_eq!(again.disposition, CommandDisposition::AlreadyApplied);
    assert_eq!(close_result(again).close_id, result.close_id);
    assert_eq!(f.state().policy, LaneManagementPolicy::Attached);
    f.stop().await;
}

#[tokio::test]
async fn the_close_preview_is_the_ledgers_and_follows_the_close_and_detach() {
    use workbench_protocol::namespaces::lanes::LaneClosePreview;
    let mut f = Fixture::new().await;
    let lane_id = workbench_protocol::lane_attention::LaneId::new(&f.lane.lane_id).unwrap();
    let session = workbench_protocol::ids::SessionId::new("wire").unwrap();
    let preview =
        |f: &Fixture| -> Result<LaneClosePreview, workbench_protocol::errors::WorkbenchError> {
            match f.host.handle_query(
                &session,
                &Query::Lanes(LanesQuery::GetLaneClosePreview {
                    lane_id: lane_id.clone(),
                }),
            )? {
                QueryReply::Lanes(LanesReply::LaneClosePreview(preview)) => Ok(*preview),
                other => panic!("{other:?}"),
            }
        };
    let summary = |f: &Fixture| match f
        .host
        .handle_query(
            &session,
            &Query::Lanes(LanesQuery::GetLane {
                lane_id: lane_id.clone(),
            }),
        )
        .unwrap()
    {
        QueryReply::Lanes(LanesReply::Lane(detail)) => detail.summary,
        other => panic!("{other:?}"),
    };

    // What the wire shows is what the ledger's own preview holds.
    let stored = f
        .storage
        .handle()
        .lane_close_context(f.lane.lane_id.clone())
        .unwrap();
    let shown = preview(&f).unwrap();
    assert_eq!(shown.management_revision.0, stored.management_revision);
    assert_eq!(shown.request_revision.0, stored.request_revision);
    assert_eq!(shown.brief_revision.0, stored.brief_revision);
    assert_eq!(shown.objective, "independent work");
    assert_eq!(shown.acceptance_criteria, ["review"]);
    assert!(shown.ready.is_none());
    assert_eq!(shown.open_question_count, 0);
    assert!(shown.running, "the lane's root run is live");
    assert!(shown.current_close.is_none());
    assert!(summary(&f).current_close.is_none());

    // A close shows as the standing record in the preview, the detail and the listing.
    let reply = f
        .host
        .handle_command(
            Command::Lanes(LanesCommand::CloseLane(CloseLane {
                lane_id: lane_id.clone(),
                kind: HumanLaneCloseKind::Discard,
                ready_seq: None,
                expected_management_revision: shown.management_revision,
                expected_request_revision: shown.request_revision,
                note: "not needed".into(),
            })),
            common::web(),
        )
        .unwrap();
    let closed = close_result(reply);
    let after = preview(&f).unwrap();
    let standing = after.current_close.expect("the close stands");
    assert_eq!(standing.close_id, closed.close_id);
    assert_eq!(standing.kind, LaneCloseRecordKind::Discarded);
    assert!(after.running, "closing stops no run");
    assert_eq!(summary(&f).current_close, Some(standing.clone()));
    let listed = match f
        .host
        .handle_query(
            &session,
            &Query::Lanes(LanesQuery::ListLanes(
                workbench_protocol::namespaces::lanes::ListLanes {
                    lead_thread: workbench_protocol::ids::ThreadId::new("lead").unwrap(),
                    policy: None,
                    detached: false,
                    page: Default::default(),
                },
            )),
        )
        .unwrap()
    {
        QueryReply::Lanes(LanesReply::Lanes(page)) => page.items,
        other => panic!("{other:?}"),
    };
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].current_close, Some(standing));

    // A new Report lifts the close: the badge is gone in the detail and the preview.
    f.storage
        .handle()
        .append_lane_report(
            &f.lane.lane_id,
            stored.management_revision,
            event_bus::LaneReportInput {
                status: event_bus::lane::LaneStatus::Working,
                summary: "still going".into(),
                artifact_revision: None,
                failure_episode: None,
                artifact_refs: vec![],
                needs_human: false,
                question_refs: vec![],
                reason_code: None,
                producer: event_bus::lane::LaneReportProducer::LaneAgent,
            },
            1,
        )
        .unwrap();
    assert!(summary(&f).current_close.is_none());
    let lifted = preview(&f).unwrap();
    assert!(lifted.current_close.is_none());
    assert!(lifted.request_revision.0 > after.request_revision.0);

    // A detached lane has no close preview, and an unknown lane is not found.
    let detach = f.command(
        "detach",
        stored.management_revision,
        LaneManagementAction::Detach,
    );
    f.host.handle_command(detach, common::web()).unwrap();
    assert_eq!(preview(&f).unwrap_err().code, ErrorCode::Conflict);
    assert!(summary(&f).current_close.is_none());
    let unknown = f.host.handle_query(
        &session,
        &Query::Lanes(LanesQuery::GetLaneClosePreview {
            lane_id: workbench_protocol::lane_attention::LaneId::new("no-such-lane").unwrap(),
        }),
    );
    assert_eq!(unknown.unwrap_err().code, ErrorCode::NotFound);
    f.stop().await;
}
