//! Read adapters of the execution and lanes areas over a real runtime, SQLite
//! and ownership: live and saved runs merge, a lane listing pages without
//! repeats, the lane ledger decides what is shown, and operations aimed at an
//! older revision, lead or run are refused without changing anything.
mod common;
use runtime::{AgentRuntime, Role, RunConfig, RunId};
use serde_json::json;
use std::collections::BTreeSet;
use std::sync::Arc;
use workbench_host::areas::{Areas, execution::ExecutionArea, lanes::LanesArea, teams::TeamsArea};
use workbench_host::{CommandHost, commands::CommandService};

fn areas(runtime: &AgentRuntime) -> Areas {
    Areas {
        execution: ExecutionArea::with_runtime(runtime.clone()),
        teams: TeamsArea::with_runtime(runtime.clone()),
        lanes: LanesArea::with_runtime(runtime.clone()),
        ..Areas::default()
    }
}
use workbench_protocol::commands::{Command, CommandDisposition, StopRun};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{PageCursor, SessionId, ThreadId};
use workbench_protocol::lane_attention::LaneId;
use workbench_protocol::lane_management::{
    LaneManagementAction, LaneManagementOperationId, LaneManagementPolicy, LaneManagementRequest,
    ManageLane, ManagedLanePolicy,
};
use workbench_protocol::namespaces::execution::{
    RunDetail, RunDetailsQuery, RunLifecycle, RunRef, RunScope, RunSource,
};
use workbench_protocol::namespaces::lanes::{LaneStartState, ListLanes, ResumeLaneDeliveries};
use workbench_protocol::namespaces::{
    ExecutionCommand, ExecutionQuery, ExecutionReply, LanesCommand, LanesQuery, LanesReply,
};
use workbench_protocol::queries::{PageRequest, Query, QueryReply};
use workbench_protocol::views::Page;
use workbench_protocol::wire::Revision;

struct Fixture {
    dir: tempfile::TempDir,
    storage: storage::Storage,
    runtime: AgentRuntime,
    owner: Arc<runtime::ownership::OwnerHost>,
    host: CommandHost,
    model: Arc<common::HeldModel>,
    lead: RunId,
    lanes: Vec<runtime::lanes::LaneOpened>,
}

fn brief(objective: &str) -> event_bus::LaneBrief {
    serde_json::from_value(json!({
        "objective": objective, "inputs": [], "deliverables": ["patch"],
        "acceptance_criteria": ["review"], "constraints": [], "workspace": {},
        "report_policy": "attached"
    }))
    .unwrap()
}

impl Fixture {
    async fn new(lane_count: usize) -> Self {
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
        let mut lanes = Vec::new();
        for n in 0..lane_count {
            let opened = runtime
                .open_lane(lead, &format!("open-{n}"), brief(&format!("objective {n}")))
                .unwrap();
            model.reached.notified().await;
            lanes.push(opened);
        }
        let mut threads = vec!["lead".to_owned()];
        threads.extend(lanes.iter().map(|lane| lane.thread_id.clone()));
        storage
            .handle()
            .upsert_threads_host(
                &threads
                    .iter()
                    .map(|id| storage::threads::ThreadUpsert {
                        thread_id: id.clone(),
                        project_id: Some("project-1".into()),
                        project_root: Some(dir.path().to_string_lossy().into_owned()),
                        ..Default::default()
                    })
                    .collect::<Vec<_>>(),
            )
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
            .with_areas(areas(&runtime));
        Self {
            dir,
            storage,
            runtime,
            owner,
            host,
            model,
            lead,
            lanes,
        }
    }

    fn query(
        &self,
        query: Query,
    ) -> Result<QueryReply, workbench_protocol::errors::WorkbenchError> {
        self.host
            .handle_query(&SessionId::new("wire").unwrap(), &query)
    }

    fn runs(
        &self,
        thread: &str,
        scope: RunScope,
        page: PageRequest,
    ) -> Result<Page<RunDetail>, workbench_protocol::errors::WorkbenchError> {
        match self.query(Query::Execution(ExecutionQuery::ListRunDetails(
            RunDetailsQuery {
                thread: ThreadId::new(thread).unwrap(),
                scope,
                page,
            },
        )))? {
            QueryReply::Execution(ExecutionReply::RunDetails(page)) => Ok(page),
            other => panic!("{other:?}"),
        }
    }

    fn lane_page(
        &self,
        policy: Option<ManagedLanePolicy>,
        page: PageRequest,
    ) -> Page<workbench_protocol::namespaces::lanes::LaneSummary> {
        match self
            .query(Query::Lanes(LanesQuery::ListLanes(ListLanes {
                lead_thread: ThreadId::new("lead").unwrap(),
                policy,
                page,
            })))
            .unwrap()
        {
            QueryReply::Lanes(LanesReply::Lanes(page)) => page,
            other => panic!("{other:?}"),
        }
    }

    fn lane_detail(&self, lane: &str) -> workbench_protocol::namespaces::lanes::LaneDetail {
        match self
            .query(Query::Lanes(LanesQuery::GetLane {
                lane_id: LaneId::new(lane).unwrap(),
            }))
            .unwrap()
        {
            QueryReply::Lanes(LanesReply::Lane(detail)) => *detail,
            other => panic!("{other:?}"),
        }
    }

    fn manage(
        &self,
        lane: usize,
        id: &str,
        revision: u64,
        action: LaneManagementAction,
    ) -> Command {
        Command::Lanes(LanesCommand::ManageLane(ManageLane {
            request: LaneManagementRequest {
                operation_id: LaneManagementOperationId::new(id).unwrap(),
                lane_id: LaneId::new(&self.lanes[lane].lane_id).unwrap(),
                expected_management_revision: Revision(revision),
                action,
            },
            expected_family_revision: None,
        }))
    }

    fn resume(&self, lane: usize, revision: u64, lead: &str, root: &str) -> Command {
        Command::Lanes(LanesCommand::ResumeLaneDeliveries(ResumeLaneDeliveries {
            lane_id: LaneId::new(&self.lanes[lane].lane_id).unwrap(),
            expected_management_revision: Revision(revision),
            lead_thread_id: ThreadId::new(lead).unwrap(),
            expected_lead_root_run_id: workbench_protocol::ids::RunId::new(root).unwrap(),
        }))
    }

    async fn stop(&self) {
        for lane in &self.lanes {
            self.runtime.cancel(lane.root_run_id).unwrap();
            self.runtime.wait(lane.root_run_id).await.unwrap();
        }
        self.runtime.cancel(self.lead).unwrap();
        self.runtime.wait(self.lead).await.unwrap();
    }
}

fn page_request(limit: u32, cursor: Option<PageCursor>) -> PageRequest {
    PageRequest { cursor, limit }
}

#[tokio::test]
async fn a_live_run_and_its_saved_record_are_one_run_and_history_outlives_the_runtime() {
    let f = Fixture::new(1).await;
    let lead = f.lead.to_string();
    let roots = f
        .runs("lead", RunScope::Roots, PageRequest::default())
        .unwrap();
    assert_eq!(roots.items.len(), 1, "live and saved are not listed twice");
    let live = &roots.items[0];
    assert_eq!(live.summary.id.as_str(), lead);
    assert_eq!(live.source, RunSource::Live);
    assert_eq!(live.lifecycle, RunLifecycle::Active);
    assert!(live.summary.parent_run.is_none());

    // The lane's root lives in its own thread and is a root there, not a child
    // of the lead's run.
    let lane_roots = f
        .runs(
            &f.lanes[0].thread_id,
            RunScope::Roots,
            PageRequest::default(),
        )
        .unwrap();
    assert_eq!(lane_roots.items.len(), 1);
    assert_eq!(
        lane_roots.items[0].summary.id.as_str(),
        f.lanes[0].root_run_id.to_string()
    );
    let children = f
        .runs(
            "lead",
            RunScope::Children {
                parent: live.summary.id.clone(),
            },
            PageRequest::default(),
        )
        .unwrap();
    assert!(children.items.is_empty());

    f.stop().await;
    // A second incarnation over the same store: nothing is live, the stopped
    // runs are still shown, once each, as history with a stop reason.
    let restored = common::runtime_on_store(f.dir.path(), &f.storage);
    let host = CommandHost::new(CommandService::new(
        restored.clone(),
        tokio::runtime::Handle::current(),
    ))
    .with_areas(areas(&restored));
    let session = SessionId::new("wire").unwrap();
    let QueryReply::Execution(ExecutionReply::RunDetails(after)) = host
        .handle_query(
            &session,
            &Query::Execution(ExecutionQuery::ListRunDetails(RunDetailsQuery {
                thread: ThreadId::new("lead").unwrap(),
                scope: RunScope::Roots,
                page: PageRequest::default(),
            })),
        )
        .unwrap()
    else {
        panic!("run details expected");
    };
    assert_eq!(after.items.len(), 1);
    let saved = &after.items[0];
    assert_eq!(saved.summary.id.as_str(), lead);
    assert_eq!(saved.source, RunSource::HistoryOnly);
    assert!(
        matches!(
            saved.lifecycle,
            RunLifecycle::Terminal | RunLifecycle::Stopped
        ),
        "{saved:?}"
    );
    assert!(saved.terminal_reason.is_some());
    assert!(saved.model.is_none());
    let QueryReply::Execution(ExecutionReply::Run(one)) = host
        .handle_query(
            &session,
            &Query::Execution(ExecutionQuery::GetRun(RunRef {
                thread: ThreadId::new("lead").unwrap(),
                run: saved.summary.id.clone(),
            })),
        )
        .unwrap()
    else {
        panic!("run expected");
    };
    assert_eq!(&*one, saved);
    let unknown = host
        .handle_query(
            &session,
            &Query::Execution(ExecutionQuery::GetRun(RunRef {
                thread: ThreadId::new("lead").unwrap(),
                run: workbench_protocol::ids::RunId::new("run-missing").unwrap(),
            })),
        )
        .unwrap_err();
    assert_eq!(unknown.code, ErrorCode::NotFound);
}

#[test]
fn a_stop_names_one_thread_and_never_reaches_a_run_of_another() {
    let rt = tokio::runtime::Runtime::new().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mut host = common::host_with_project(&rt, common::runtime(), dir.path());
    let started: Vec<_> =
        ["thread-1", "thread-2"]
            .iter()
            .map(|thread| {
                host.handle_command(
                    Command::Conversation(
                        workbench_protocol::namespaces::ConversationCommand::SendChat(
                            common::send(thread, &format!("sub-{thread}"), "start"),
                        ),
                    ),
                    common::web(),
                )
                .unwrap()
                .run_id
                .expect("a started chat names its run")
            })
            .collect();
    let live = |host: &CommandHost| -> BTreeSet<String> {
        host.service()
            .runtime()
            .list_agents()
            .iter()
            .map(|agent| agent.run_id.to_string())
            .collect()
    };
    assert!(started.iter().all(|run| live(&host).contains(run.as_str())));

    // thread-1 is asked to stop a run of thread-2: refused, and nothing stops.
    let refused = host
        .handle_command(
            Command::Execution(ExecutionCommand::StopRun(StopRun {
                thread: ThreadId::new("thread-1").unwrap(),
                run: started[1].clone(),
            })),
            common::web(),
        )
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::Conflict);
    assert!(started.iter().all(|run| live(&host).contains(run.as_str())));

    // Stopping thread-1 stops its own run and leaves thread-2's alone.
    host.handle_command(
        Command::Execution(ExecutionCommand::StopChat(
            workbench_protocol::commands::ThreadTarget {
                thread: ThreadId::new("thread-1").unwrap(),
            },
        )),
        common::web(),
    )
    .unwrap();
    let first: runtime::RunId = started[0].as_str().parse().unwrap();
    let _ = rt.block_on(host.service().runtime().wait(first));
    let agents = host.service().runtime().list_agents();
    let phase_of = |run: &str| {
        agents
            .iter()
            .find(|agent| agent.run_id.to_string() == run)
            .map(|agent| agent.phase)
    };
    assert_eq!(
        phase_of(started[0].as_str()),
        Some(event_bus::AgentRunPhase::Stopped)
    );
    assert!(matches!(
        phase_of(started[1].as_str()),
        Some(event_bus::AgentRunPhase::Running | event_bus::AgentRunPhase::Pending)
    ));
}

#[tokio::test]
async fn the_lane_listing_pages_without_repeats_and_follows_the_ledger() {
    let mut f = Fixture::new(3).await;
    let first = f.lane_page(None, page_request(2, None));
    assert_eq!(first.items.len(), 2);
    let cursor = first.next_cursor.clone().expect("a second page follows");
    let second = f.lane_page(None, page_request(2, Some(cursor)));
    assert_eq!(second.items.len(), 1);
    assert!(second.next_cursor.is_none());
    let seen: BTreeSet<_> = first
        .items
        .iter()
        .chain(&second.items)
        .map(|lane| lane.lane_id.as_str().to_owned())
        .collect();
    let expected: BTreeSet<_> = f.lanes.iter().map(|lane| lane.lane_id.clone()).collect();
    assert_eq!(seen, expected, "every lane exactly once");
    assert_eq!(first.snapshot_revision, second.snapshot_revision);

    // A cursor belongs to its own query: another policy filter refuses it.
    let mismatched = f
        .query(Query::Lanes(LanesQuery::ListLanes(ListLanes {
            lead_thread: ThreadId::new("lead").unwrap(),
            policy: Some(ManagedLanePolicy::Summary),
            page: page_request(2, first.next_cursor.clone()),
        })))
        .unwrap_err();
    assert_eq!(mismatched.code, ErrorCode::Conflict);

    // Policy comes from the ledger: a summary lane is listed under Summary only.
    let target = f.lanes[0].lane_id.clone();
    let reply = f
        .host
        .handle_command(
            f.manage(
                0,
                "policy-1",
                1,
                LaneManagementAction::SetPolicy {
                    policy: ManagedLanePolicy::Summary,
                },
            ),
            common::web(),
        )
        .unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    let summary_only = f.lane_page(Some(ManagedLanePolicy::Summary), PageRequest::default());
    assert_eq!(summary_only.items.len(), 1);
    assert_eq!(summary_only.items[0].lane_id.as_str(), target);
    assert_eq!(summary_only.items[0].policy, LaneManagementPolicy::Summary);
    assert_eq!(
        f.lane_page(Some(ManagedLanePolicy::Attached), PageRequest::default())
            .items
            .len(),
        2
    );

    // Detaching removes the lane from the lead's listing, yet the lane can still
    // be read (with its detached revision) and an older revision is refused.
    let stale = f
        .host
        .handle_command(
            f.manage(0, "stale-detach", 1, LaneManagementAction::Detach),
            common::web(),
        )
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::StaleRevision);
    f.host
        .handle_command(
            f.manage(0, "detach", 2, LaneManagementAction::Detach),
            common::web(),
        )
        .unwrap();
    let remaining = f.lane_page(None, PageRequest::default());
    assert_eq!(remaining.items.len(), 2);
    assert!(
        remaining
            .items
            .iter()
            .all(|lane| lane.lane_id.as_str() != target)
    );
    let detail = f.lane_detail(&target);
    assert_eq!(detail.summary.policy, LaneManagementPolicy::Detached);
    assert_eq!(detail.summary.start, LaneStartState::Started);
    assert!(detail.detached_since_management_revision.is_some());
    assert!(detail.summary.lead_thread_id.is_none());

    let missing = f
        .query(Query::Lanes(LanesQuery::GetLane {
            lane_id: LaneId::new("lane-missing").unwrap(),
        }))
        .unwrap_err();
    assert_eq!(missing.code, ErrorCode::NotFound);
    f.stop().await;
}

#[tokio::test]
async fn a_held_delivery_is_resumed_only_for_the_current_revision_lead_and_root() {
    let mut f = Fixture::new(1).await;
    let lead_root = f.lead.to_string();
    let revision = f
        .lane_detail(&f.lanes[0].lane_id)
        .summary
        .management_revision
        .0;

    // An agent cannot resume deliveries: it is a human action.
    let agent = f
        .host
        .handle_command(
            f.resume(0, revision, "lead", &lead_root),
            Provenance::agent(workbench_protocol::ids::RunId::new(lead_root.clone()).unwrap()),
        )
        .unwrap_err();
    assert_eq!(agent.code, ErrorCode::PermissionDenied);
    // An older management revision, another lead and another root run are
    // refused, each without resuming anything.
    let stale = f
        .host
        .handle_command(f.resume(0, revision + 1, "lead", &lead_root), common::web())
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::StaleRevision);
    let other_lead = f
        .host
        .handle_command(
            f.resume(0, revision, "someone-else", &lead_root),
            common::web(),
        )
        .unwrap_err();
    assert_eq!(other_lead.code, ErrorCode::PermissionDenied);
    let other_root = f
        .host
        .handle_command(
            f.resume(0, revision, "lead", &f.lanes[0].root_run_id.to_string()),
            common::web(),
        )
        .unwrap_err();
    assert!(matches!(
        other_root.code,
        ErrorCode::StaleRevision | ErrorCode::Conflict
    ));
    // A detached lane has no current lead to resume for.
    f.host
        .handle_command(
            f.manage(0, "detach", revision, LaneManagementAction::Detach),
            common::web(),
        )
        .unwrap();
    let detached = f
        .host
        .handle_command(f.resume(0, revision + 1, "lead", &lead_root), common::web())
        .unwrap_err();
    assert_eq!(detached.code, ErrorCode::PermissionDenied);
    // The lead keeps running through all of it.
    assert!(f.runtime.list_agents().iter().any(|a| a.run_id == f.lead));
    let _ = (&f.owner, &f.model);
    f.stop().await;
}

#[tokio::test]
async fn a_stopped_lead_keeps_its_lanes_and_a_resume_does_not_restart_it() {
    let mut f = Fixture::new(1).await;
    let lead_root = f.lead.to_string();
    let revision = f
        .lane_detail(&f.lanes[0].lane_id)
        .summary
        .management_revision
        .0;
    f.runtime.cancel(f.lead).unwrap();
    let _ = f.runtime.wait(f.lead).await;

    // The ledger, not the lead's liveness, decides what is managed.
    let page = f.lane_page(None, PageRequest::default());
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].policy, LaneManagementPolicy::Attached);
    // Naming the lead's current root run is accepted, and it is only a delivery
    // of held notices: the stopped lead is not started again by it.
    let reply = f
        .host
        .handle_command(f.resume(0, revision, "lead", &lead_root), common::web())
        .unwrap();
    assert_eq!(reply.disposition, CommandDisposition::Accepted);
    assert!(
        f.runtime
            .list_agents()
            .iter()
            .filter(|agent| agent.run_id == f.lead)
            .all(|agent| matches!(
                agent.phase,
                event_bus::AgentRunPhase::Stopped
                    | event_bus::AgentRunPhase::Done
                    | event_bus::AgentRunPhase::Error
            ))
    );
    assert_eq!(f.lane_page(None, PageRequest::default()).items, page.items);
    f.runtime.cancel(f.lanes[0].root_run_id).unwrap();
    let _ = f.runtime.wait(f.lanes[0].root_run_id).await;
}
