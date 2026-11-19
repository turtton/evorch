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
            capture_budget: Arc::new(tokio::sync::Semaphore::new(MAX_CAPTURE_BYTES)),
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
            slot: None,
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

/// The Web surface may read metadata only: even with the owner's client id it can
/// neither start frame delivery nor take a Capture, so it never holds a lease or
/// an operation a data connection could bind (F3 sends bodies to the desktop
/// and CLI surfaces only; Web support is a separate plan).
#[tokio::test]
async fn the_web_surface_cannot_obtain_frames_or_a_capture_even_as_the_owner() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let web = provenance("owner", Surface::Web);
    let (session, _) = f.session(&owner);
    let tab = live_tab(&session, owner.client_id.clone());
    let reference = tab.public.reference.clone();
    f.service
        .state
        .lock()
        .unwrap()
        .tabs
        .insert(reference.id.as_str().to_owned(), tab);
    for command in [
        BrowserCommand::SubscribeRendererFrames {
            tab: reference.clone(),
        },
        BrowserCommand::SetRendererStreaming {
            tab: reference.clone(),
            enabled: true,
        },
        BrowserCommand::CaptureRendererTab {
            tab: reference.clone(),
        },
    ] {
        let error = f.command(&web, command).unwrap_err();
        assert_eq!(error.code, ErrorCode::PermissionDenied);
    }
    {
        let state = f.service.state.lock().unwrap();
        assert!(state.subscriptions.is_empty());
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

/// A frame connection the service can lease from. No helper runs in these
/// tests, so the peers of its two channels are kept here and never speak.
struct FramePeer {
    _control: std::os::unix::net::UnixStream,
    _frames: std::os::unix::net::UnixStream,
}
fn install_frame_source(f: &Fixture) -> FramePeer {
    let (control, peer_control) = std::os::unix::net::UnixStream::pair().unwrap();
    let (frames, peer_frames) = std::os::unix::net::UnixStream::pair().unwrap();
    // The reader jobs are never started: nothing reads these frames.
    let (client, _jobs) = renderer_ipc::RendererClient::prepare(
        control,
        frames,
        renderer_ipc::ClientConfig::new(renderer_ipc::RendererGeneration(1)),
    )
    .unwrap();
    f.service.owner.install_ready_helper(1, Arc::new(client));
    FramePeer {
        _control: peer_control,
        _frames: peer_frames,
    }
}
/// Inserts a tab of the viewport `(width, height)` at 30fps that `owner` owns.
fn insert_tab(
    f: &Fixture,
    session: &RendererSessionRef,
    owner: &Provenance,
    physical: u64,
    (width, height): (u32, u32),
) -> RendererTabRef {
    let mut tab = live_tab(session, owner.client_id.clone());
    tab.physical.id = renderer_ipc::TabId(physical);
    tab.public.viewport = RendererViewport { width, height };
    let reference = tab.public.reference.clone();
    f.service
        .state
        .lock()
        .unwrap()
        .tabs
        .insert(reference.id.as_str().to_owned(), tab);
    reference
}
fn output(f: &Fixture, p: &Provenance, operation_id: RendererId) -> RendererOutput {
    let BrowserReply::Operation(RendererOperation {
        state: RendererOperationState::Completed { output },
        ..
    }) = f
        .service
        .query(&BrowserQuery::GetRendererOperation { operation_id }, p)
        .unwrap()
    else {
        panic!("the operation did not complete")
    };
    output
}
/// The public commands a client sends to read a tab's frames.
struct Frames<'a> {
    f: &'a Fixture,
    owner: &'a Provenance,
}
impl Frames<'_> {
    fn subscribe(&self, tab: &RendererTabRef) -> Result<(RendererId, RendererId), WorkbenchError> {
        let op = self.f.command(
            self.owner,
            BrowserCommand::SubscribeRendererFrames { tab: tab.clone() },
        )?;
        let RendererOutput::Subscription {
            subscription,
            token,
            ..
        } = output(self.f, self.owner, op)
        else {
            panic!("not a subscription")
        };
        Ok((subscription, token))
    }
    fn unsubscribe(&self, subscription: &RendererId) {
        self.f
            .command(
                self.owner,
                BrowserCommand::UnsubscribeRendererFrames {
                    subscription: subscription.clone(),
                },
            )
            .unwrap();
    }
    /// Opens a data channel and binds the subscription to it through the
    /// control command, as a client does. The lease is what the data
    /// connection then holds.
    async fn bind(
        &self,
        (subscription, token): (RendererId, RendererId),
    ) -> crate::areas::browser::renderer::data::Lease {
        let channel = self.f.service.owner.data_plane().open_channel().unwrap();
        self.f
            .command(
                self.owner,
                BrowserCommand::BindRendererData {
                    channel_id: channel.id().clone(),
                    source: RendererDataSource::Frames {
                        subscription,
                        token,
                    },
                },
            )
            .unwrap();
        channel.bound().await.expect("the channel was bound")
    }
}
fn refusal(result: Result<(RendererId, RendererId), WorkbenchError>) -> (ErrorCode, RetryAdvice) {
    let error = result.expect_err("the subscription must be refused");
    (error.code, error.retry)
}

#[tokio::test(start_paused = true)]
async fn a_subscription_place_is_held_until_its_lease_lets_go() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let _connection = f.service.connections.open(owner.client_id.clone().unwrap());
    let _peer = install_frame_source(&f);
    let (session, _) = f.session(&owner);
    let tab = insert_tab(&f, &session, &owner, 1, (1024, 768));
    let frames = Frames {
        f: &f,
        owner: &owner,
    };
    let mut subscriptions = (0..data::MAX_FRAME_SUBSCRIPTIONS)
        .map(|_| frames.subscribe(&tab).unwrap())
        .collect::<Vec<_>>();
    let general = (ErrorCode::Busy, RetryAdvice::Never);
    assert_eq!(refusal(frames.subscribe(&tab)), general);

    // A subscription that never got a data connection gives its place back
    // when it ends.
    let unbound = subscriptions.pop().unwrap();
    frames.unsubscribe(&unbound.0);
    let replacement = frames.subscribe(&tab).unwrap();
    assert_eq!(refusal(frames.subscribe(&tab)), general);
    subscriptions.push(replacement);

    // Two subscriptions end while their leases (and so their sockets) are
    // still closing: the leases keep the places, whatever order they go in.
    let (first, second) = (subscriptions.pop().unwrap(), subscriptions.pop().unwrap());
    let (first_subscription, second_subscription) = (first.0.clone(), second.0.clone());
    let lease_a = frames.bind(first).await;
    let lease_b = frames.bind(second).await;
    frames.unsubscribe(&first_subscription);
    frames.unsubscribe(&second_subscription);
    assert!(f.service.state.lock().unwrap().subscriptions.len() < data::MAX_FRAME_SUBSCRIPTIONS);
    assert_eq!(refusal(frames.subscribe(&tab)), general);
    drop(lease_b);
    let one = frames.subscribe(&tab).unwrap();
    assert_eq!(refusal(frames.subscribe(&tab)), general);
    drop(lease_a);
    let two = frames.subscribe(&tab).unwrap();
    assert_eq!(refusal(frames.subscribe(&tab)), general);
    // What is left is the limit, not a leaked place.
    assert_eq!(
        f.service.state.lock().unwrap().subscriptions.len(),
        data::MAX_FRAME_SUBSCRIPTIONS
    );
    drop((one, two));
    f.finish().await;
}

/// A 4K stream has four subscriptions of its own, counted from the live
/// subscriptions of its tab. The places of the core-wide limit are still held
/// by a lease that outlives its subscription, and the refusal then is the
/// general one, not the 4K advice.
#[tokio::test(start_paused = true)]
async fn a_four_k_subscription_limit_and_its_lease_release_are_counted_separately() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let _connection = f.service.connections.open(owner.client_id.clone().unwrap());
    let _peer = install_frame_source(&f);
    let (session, _) = f.session(&owner);
    let four_k = insert_tab(&f, &session, &owner, 1, (3840, 2160));
    let other = insert_tab(&f, &session, &owner, 2, (1024, 768));
    let frames = Frames {
        f: &f,
        owner: &owner,
    };
    let mut live = (0..data::MAX_FOUR_K_FRAME_SUBSCRIPTIONS)
        .map(|_| frames.subscribe(&four_k).unwrap())
        .collect::<Vec<_>>();
    let four_k_busy = (ErrorCode::Busy, RetryAdvice::AfterResync);
    assert_eq!(refusal(frames.subscribe(&four_k)), four_k_busy);
    // The other tab takes the rest of the core-wide places.
    let rest = (data::MAX_FOUR_K_FRAME_SUBSCRIPTIONS..data::MAX_FRAME_SUBSCRIPTIONS)
        .map(|_| frames.subscribe(&other).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        refusal(frames.subscribe(&other)),
        (ErrorCode::Busy, RetryAdvice::Never)
    );

    // A 4K subscription ends but its lease still holds the core-wide place:
    // the tab's own count has room, the core does not.
    let ended = live.pop().unwrap();
    let ended_subscription = ended.0.clone();
    let lease = frames.bind(ended).await;
    frames.unsubscribe(&ended_subscription);
    assert_eq!(
        refusal(frames.subscribe(&four_k)),
        (ErrorCode::Busy, RetryAdvice::Never)
    );
    drop(lease);
    live.push(frames.subscribe(&four_k).unwrap());
    // The four are back, so the 4K limit speaks again.
    assert_eq!(refusal(frames.subscribe(&four_k)), four_k_busy);
    drop((live, rest));
    f.finish().await;
}

#[tokio::test(start_paused = true)]
async fn a_revoked_capture_keeps_its_budget_until_the_last_transfer_lets_go() {
    let f = Fixture::new();
    let owner = provenance("owner", Surface::Desktop);
    let client = owner.client_id.clone().unwrap();
    let connection = f.service.connections.open(client.clone());
    let budget = f.service.capture_budget.clone();
    let acquire = |len: usize| {
        budget
            .clone()
            .try_acquire_many_owned(u32::try_from(len).unwrap())
            .ok()
    };
    // The rest of the budget is taken by other Captures, so what the stored
    // Capture holds is all that could be given back.
    let blob = workbench_protocol::namespaces::browser::data::MAX_CAPTURE_BLOB_BYTES as usize;
    let _others = acquire(MAX_CAPTURE_BYTES - blob).unwrap();
    // A Capture of the largest size a data connection may carry, as the
    // capture command stores it.
    let operation = id();
    let capture_id = id();
    f.service.state.lock().unwrap().captures.insert(
        capture_id.as_str().to_owned(),
        Capture {
            tab: Tab {
                generation: 1,
                id: renderer_ipc::TabId(1),
            },
            bytes: Arc::new(HeldBytes {
                bytes: vec![0; blob],
                _share: acquire(blob).unwrap(),
            }),
            expires: Instant::now() + GRACE,
            operation: operation.clone(),
            connection_epoch: Some(connection.epoch()),
            client: client.clone(),
            surface: owner.surface,
            bound: None,
        },
    );
    assert!(acquire(1).is_none(), "the stored Capture is counted");
    // A data connection binds it through the real binding path and keeps its lease.
    let channel = f.service.owner.data_plane().open_channel().unwrap();
    let channel_id = channel.id().clone();
    f.service
        .bind_data(
            &mut f.service.state.lock().unwrap(),
            &channel_id,
            &RendererDataSource::Capture {
                operation_id: operation,
                capture_id: capture_id.clone(),
            },
            &client,
            &owner,
        )
        .unwrap();
    let lease = channel.bound().await.expect("the channel was bound");
    // The Capture is revoked (its control connection ends) while the lease
    // still holds the bytes: the budget stays taken, so another Capture is refused.
    drop(connection);
    assert!(f.service.state.lock().unwrap().captures.is_empty());
    assert!(
        acquire(1).is_none(),
        "the lease still holds the revoked Capture's bytes"
    );
    drop(lease);
    assert!(acquire(blob).is_some());
    f.finish().await;
}
