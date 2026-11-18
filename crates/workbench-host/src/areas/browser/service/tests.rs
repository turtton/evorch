use super::*;
use crate::{
    areas::browser::renderer::OwnerConfig, bootstrap::restart::CommandAccess,
    projection::Projection,
};
use workbench_protocol::ids::{ProjectId, ThreadId};

struct NoModel;
#[async_trait::async_trait]
impl runtime::AgentModel for NoModel {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "unused-renderer-model".into()
    }
}
struct Fixture {
    _directory: tempfile::TempDir,
    _projection: Arc<Projection>,
    ledger: sandbox::SpawnLedger,
    service: Arc<RendererService>,
}
impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let handle = tokio::runtime::Handle::current();
        let tasks = AreaTasks::new(handle.clone());
        let ledger = sandbox::SpawnLedger::new();
        let owner = RendererOwner::new(
            &tasks,
            ledger.clone(),
            OwnerConfig::new(
                directory.path().join("not-installed"),
                directory.path().join("profiles"),
            ),
        )
        .unwrap();
        let bus = Arc::new(event_bus::EventBus::new(64));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(NoModel),
        );
        let projection = Projection::new(runtime, &handle, Default::default());
        let service = Arc::new(RendererService {
            epoch: projection.instance_epoch().get(),
            owner,
            store: Arc::new(
                runtime::artifacts::ArtifactStore::new(directory.path().join("artifacts")).unwrap(),
            ),
            tasks,
            gate: HostRestartGate::default(),
            connections: Arc::default(),
            source: projection.source("browser").unwrap(),
            state: Mutex::default(),
        });
        service.start().unwrap();
        Self {
            _directory: directory,
            _projection: projection,
            ledger,
            service,
        }
    }
    fn command(
        &self,
        p: &Provenance,
        command: BrowserCommand,
    ) -> Result<RendererId, WorkbenchError> {
        self.service.command(
            command,
            p,
            self.service.gate.enter(CommandAccess::NewWork).unwrap(),
        )
    }
    fn session(&self, p: &Provenance) -> (RendererSessionRef, RendererId) {
        let op = self
            .command(p, BrowserCommand::CreateRendererSession {})
            .unwrap();
        let BrowserReply::Operation(RendererOperation {
            state:
                RendererOperationState::Completed {
                    output:
                        RendererOutput::Session {
                            session,
                            resume_token,
                        },
                },
            ..
        }) = self
            .service
            .query(&BrowserQuery::GetRendererOperation { operation_id: op }, p)
            .unwrap()
        else {
            panic!("create session did not complete")
        };
        (session, resume_token)
    }
    async fn finish(self) {
        assert!(
            self.ledger.children().is_empty(),
            "private state must not eagerly spawn a helper"
        );
        self.ledger.close_fence();
        self.service.tasks.request_stop();
        self.service.tasks.shutdown().await.unwrap();
        self.ledger.join_workers().await.unwrap();
    }
}
fn provenance(client: &str, surface: Surface) -> Provenance {
    Provenance {
        actor: Actor::Human,
        surface,
        client_id: Some(ClientId::new(client).unwrap()),
        agent_run_id: None,
    }
}
fn live_tab(session: &RendererSessionRef, client: Option<ClientId>) -> LiveTab {
    LiveTab {
        physical: Tab {
            generation: 1,
            id: renderer_ipc::TabId(1),
        },
        client,
        observers: HashMap::default(),
        public: RendererTab {
            reference: RendererTabRef {
                core_epoch: session.core_epoch,
                generation: 1,
                id: id(),
            },
            session: session.clone(),
            owner: RendererOwnerKind::ClientPreview,
            artifact: ArtifactRef {
                project: ProjectId::new("project").unwrap(),
                thread: ThreadId::new("thread").unwrap(),
                artifact_id: format!("artifact-{}", "a".repeat(32)),
                digest: "b".repeat(64),
            },
            viewport: RendererViewport {
                width: 1024,
                height: 768,
            },
            fps: 30,
            streaming: false,
        },
    }
}

#[tokio::test]
async fn opaque_session_and_operation_ids_never_replace_transport_authorization() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let other = provenance("other", Surface::Desktop);
    let (session, _) = f.session(&owner);
    let operation = f
        .service
        .reserve(
            &mut f.service.state.lock().unwrap(),
            &owner,
            Some(session.id.as_str().to_owned()),
            None,
        )
        .unwrap();
    let result =
        serde_json::json!({"private": "a CDP result that must not enter events or receipts"});
    f.service.complete(
        &operation,
        Ok(RendererOutput::Cdp {
            result: result.clone(),
        }),
    );
    let query = BrowserQuery::GetRendererOperation {
        operation_id: operation,
    };
    let BrowserReply::Operation(RendererOperation {
        state:
            RendererOperationState::Completed {
                output: RendererOutput::Cdp { result: actual },
            },
        ..
    }) = f.service.query(&query, &owner).unwrap()
    else {
        panic!("private result missing")
    };
    assert_eq!(actual, result);
    for p in [
        other.clone(),
        provenance("owner", Surface::Cli),
        provenance("owner", Surface::Web),
    ] {
        assert_eq!(
            f.service.query(&query, &p).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
        assert!(
            f.command(
                &p,
                BrowserCommand::CloseRendererSession {
                    session: session.clone()
                }
            )
            .is_err()
        );
    }
    let mut stale = session.clone();
    stale.core_epoch += 1;
    assert!(
        f.command(
            &owner,
            BrowserCommand::CloseRendererSession { session: stale }
        )
        .is_err()
    );
    f.service.remove_session(
        &mut f.service.state.lock().unwrap(),
        session.id.as_str(),
        RendererFailure::Closed,
    );
    let BrowserReply::Operation(RendererOperation { state, .. }) =
        f.service.query(&query, &owner).unwrap()
    else {
        panic!("terminal summary missing")
    };
    assert_eq!(
        state,
        RendererOperationState::Cancelled {
            reason: RendererFailure::Closed
        }
    );
    assert!(f.service.query(&query, &other).is_err());
    f.finish().await;
}

#[tokio::test(start_paused = true)]
async fn disconnect_revokes_frame_leases_and_resume_rotates_token_without_transferring_owner() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let client = owner.client_id.clone().unwrap();
    let connection = f.service.connections.open(client.clone());
    let (session, token) = f.session(&owner);
    let lease_id = id();
    f.service.state.lock().unwrap().subscriptions.insert(
        lease_id.as_str().to_owned(),
        Subscription {
            client: client.clone(),
            surface: owner.surface,
            connection_epoch: Some(connection.epoch()),
            session: Some(session.id.as_str().to_owned()),
            tab: Tab {
                generation: 1,
                id: renderer_ipc::TabId(1),
            },
            expires: Instant::now() + GRACE,
            token: id(),
            bound: None,
        },
    );
    assert_eq!(f.service.reconnect_grace().until, None);
    let detached_at = Instant::now();
    drop(connection);
    // The disconnect starts one grace period; nobody is told which session.
    assert_eq!(f.service.reconnect_grace().until, Some(detached_at + GRACE));
    assert!(f.service.state.lock().unwrap().subscriptions.is_empty());
    assert!(
        f.service
            .session(&f.service.state.lock().unwrap(), &session, &owner)
            .is_err()
    );
    let connection = f.service.connections.open(client);
    for p in [
        provenance("other", Surface::Desktop),
        provenance("owner", Surface::Cli),
    ] {
        assert!(
            f.command(
                &p,
                BrowserCommand::ResumeRendererSession {
                    session: session.clone(),
                    token: token.clone()
                }
            )
            .is_err()
        );
    }
    let op = f
        .command(
            &owner,
            BrowserCommand::ResumeRendererSession {
                session: session.clone(),
                token: token.clone(),
            },
        )
        .unwrap();
    let BrowserReply::Operation(RendererOperation {
        state:
            RendererOperationState::Completed {
                output: RendererOutput::Session { resume_token, .. },
            },
        ..
    }) = f
        .service
        .query(
            &BrowserQuery::GetRendererOperation { operation_id: op },
            &owner,
        )
        .unwrap()
    else {
        panic!("resume did not complete")
    };
    assert_ne!(resume_token, token);
    // A resumed session is attached again and retains nothing.
    assert_eq!(f.service.reconnect_grace().until, None);
    drop(connection);
    assert!(f.service.reconnect_grace().until.is_some());
    let _connection = f.service.connections.open(owner.client_id.clone().unwrap());
    assert!(
        f.command(
            &owner,
            BrowserCommand::ResumeRendererSession {
                session: session.clone(),
                token
            }
        )
        .is_err()
    );
    tokio::time::advance(GRACE).await;
    // At the deadline the grace is over, whether or not the sweep has run.
    assert_eq!(f.service.reconnect_grace().until, None);
    assert!(
        f.command(
            &owner,
            BrowserCommand::ResumeRendererSession {
                session: session.clone(),
                token: resume_token
            }
        )
        .is_err()
    );
    assert!(f.service.state.lock().unwrap().sessions.is_empty());
    f.finish().await;
}

#[tokio::test]
async fn metadata_observation_never_grants_mutation_or_cross_surface_access() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let observer = provenance("observer", Surface::Desktop);
    let (session, _) = f.session(&owner);
    let mut tab = live_tab(&session, owner.client_id.clone());
    tab.observers
        .insert(observer.client_id.clone().unwrap(), (true, false));
    let reference = tab.public.reference.clone();
    {
        let mut state = f.service.state.lock().unwrap();
        state.tabs.insert(reference.id.as_str().to_owned(), tab);
        assert!(f.service.tab(&state, &reference, &owner, true).is_ok());
        assert!(f.service.tab(&state, &reference, &observer, false).is_ok());
        assert!(f.service.tab(&state, &reference, &observer, true).is_err());
        assert!(
            f.service
                .tab(
                    &state,
                    &reference,
                    &provenance("other", Surface::Desktop),
                    false
                )
                .is_err()
        );
        let mut stale = reference.clone();
        stale.generation += 1;
        assert!(f.service.tab(&state, &stale, &owner, true).is_err());
        let mut stale = reference;
        stale.core_epoch += 1;
        assert!(f.service.tab(&state, &stale, &owner, true).is_err());
    }
    f.finish().await;
}

#[tokio::test(start_paused = true)]
async fn private_operation_count_bytes_and_ttl_are_bounded_and_late_completion_stays_cancelled() {
    let f = Fixture::new();
    let p = provenance("owner", Surface::Desktop);
    let mut operations = Vec::new();
    for _ in 0..MAX_RENDERER_OPERATIONS {
        operations.push(
            f.service
                .reserve(&mut f.service.state.lock().unwrap(), &p, None, None)
                .unwrap(),
        );
    }
    assert_eq!(
        f.service
            .reserve(&mut f.service.state.lock().unwrap(), &p, None, None)
            .unwrap_err()
            .code,
        ErrorCode::Busy
    );
    let op = &operations[0];
    f.service.complete(
        op,
        Ok(RendererOutput::Cdp {
            result: serde_json::json!({"oversized": "x".repeat(MAX_CDP_RESULT_BYTES)}),
        }),
    );
    let BrowserReply::Operation(RendererOperation { state, .. }) = f
        .service
        .query(
            &BrowserQuery::GetRendererOperation {
                operation_id: op.clone(),
            },
            &p,
        )
        .unwrap()
    else {
        panic!("no bounded result")
    };
    assert_eq!(
        state,
        RendererOperationState::Failed {
            reason: RendererFailure::ResultTooLarge
        }
    );
    f.service.complete(
        &operations[1],
        Ok(RendererOutput::Cdp {
            result: serde_json::json!({"still": "usable"}),
        }),
    );
    assert!(matches!(
        f.service
            .state
            .lock()
            .unwrap()
            .operations
            .get(operations[1].as_str())
            .unwrap()
            .public
            .state,
        RendererOperationState::Completed { .. }
    ));
    f.service.clear_previews(RendererFailure::Restart);
    f.service.complete(
        &operations[2],
        Ok(RendererOutput::Cdp {
            result: serde_json::json!({"late": "must be discarded"}),
        }),
    );
    assert_eq!(
        f.service
            .state
            .lock()
            .unwrap()
            .operations
            .get(operations[2].as_str())
            .unwrap()
            .public
            .state,
        RendererOperationState::Cancelled {
            reason: RendererFailure::Restart
        }
    );
    tokio::time::advance(RESULT_TTL).await;
    f.service.prune();
    assert!(f.service.state.lock().unwrap().operations.is_empty());
    assert!(
        f.service
            .reserve(&mut f.service.state.lock().unwrap(), &p, None, None)
            .is_ok()
    );
    f.finish().await;
}
