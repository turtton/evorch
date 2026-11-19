use super::*;
use crate::areas::browser::observation::{ObservedFrame, fake::FakeObservationPort};
use workbench_protocol::ids::{ProjectId, RunId, ThreadId};
fn scope() -> ObservationScope {
    ObservationScope {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        run: RunId::new("run").unwrap(),
        incarnation: RunId::new("incarnation").unwrap(),
        core_epoch: 7,
        browser_generation: 1,
        page_generation: 1,
    }
}
fn page() -> ObservedPage {
    let scope = scope();
    let tab = id();
    ObservedPage {
        snapshot: ObservationSnapshot {
            scope: scope.clone(),
            revision: 1,
            state: ObservationState::Live,
            tabs: vec![ObservationTab {
                id: tab.clone(),
                title: "title".into(),
                url: "https://example.test/private".into(),
            }],
            active_tab: Some(tab.clone()),
            activity: vec![],
        },
        frame: Some(Arc::new(ObservedFrame {
            start: data::AgentFrameStart {
                scope,
                tab,
                sequence: 1,
                width: 1,
                height: 1,
                stride: 4,
                total_bytes: 4,
            },
            rgba: Arc::from([1, 2, 3, 255]),
        })),
    }
}
#[tokio::test(start_paused = true)]
async fn unused_subscription_expires_at_the_common_bind_window() {
    let port = Arc::new(FakeObservationPort::default());
    assert!(port.publish(page()));
    let plane = DataPlane::default();
    let connections = Arc::new(ConnectionHooks::default());
    let client = ClientId::new("desktop").unwrap();
    let _connection = connections.open(client.clone());
    let p = Provenance::human(Surface::Desktop, client);
    let service = ObservationService::new(Some(port), plane.clone(), connections, 7);
    let ObservationResult::Subscribed { subscription, .. } = service
        .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
        .unwrap()
    else {
        panic!()
    };
    tokio::time::advance(std::time::Duration::from_millis(u64::from(
        workbench_protocol::namespaces::browser::data::BIND_WINDOW_MS,
    )))
    .await;
    assert!(
        service
            .query(&BrowserQuery::GetAgentSubscription { subscription }, &p)
            .is_err()
    );
    let slots = (0..8)
        .map(|_| plane.frame_slot().unwrap())
        .collect::<Vec<_>>();
    assert!(plane.frame_slot().is_none());
    drop(slots);
}
#[tokio::test]
async fn fake_query_subscribe_bind_receive_cancel_and_closing_slot() {
    let port = Arc::new(FakeObservationPort::default());
    assert!(port.publish(page()));
    let plane = DataPlane::default();
    let connections = Arc::new(ConnectionHooks::default());
    let client = ClientId::new("desktop").unwrap();
    let _connection = connections.open(client.clone());
    let p = Provenance::human(Surface::Desktop, client);
    let service = ObservationService::new(Some(port.clone()), plane.clone(), connections, 7);
    assert!(
        matches!(service.query(&BrowserQuery::ListAgentRuns{project:scope().project,thread:scope().thread},&p).unwrap(),ObservationReply::Runs{scopes} if scopes==vec![scope()])
    );
    let ObservationResult::Subscribed { subscription, .. } = service
        .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
        .unwrap()
    else {
        panic!()
    };
    let ObservationReply::Subscription {
        token: Some(token),
        state: ObservationState::Starting,
        ..
    } = service
        .query(
            &BrowserQuery::GetAgentSubscription {
                subscription: subscription.clone(),
            },
            &p,
        )
        .unwrap()
    else {
        panic!()
    };
    let channel = plane.open_channel().unwrap();
    let channel_id = channel.id().clone();
    let bind = BrowserCommand::BindAgentData {
        subscription: subscription.clone(),
        token,
        channel_id,
    };
    service.command(bind.clone(), &p).unwrap();
    assert!(service.command(bind, &p).is_err());
    let Lease::AgentFrames(mut lease) = channel.bound().await.unwrap() else {
        panic!()
    };
    assert_eq!(&*lease.next().await.unwrap().rgba, &[1, 2, 3, 255]);
    // Renderer and agent observation draw from the SAME eight slots.
    let renderer_slots = (0..7)
        .map(|_| plane.frame_slot().unwrap())
        .collect::<Vec<_>>();
    assert!(plane.frame_slot().is_none());
    service
        .command(BrowserCommand::UnsubscribeAgentFrames { subscription }, &p)
        .unwrap();
    assert!(!lease.current());
    assert!(lease.next().await.is_none());
    assert!(plane.frame_slot().is_none(), "closing lease still counts");
    drop(lease);
    assert!(plane.frame_slot().is_some());
    drop(renderer_slots);
}
#[tokio::test]
async fn revoked_scope_wrong_client_surface_and_epoch_are_refused() {
    let port = Arc::new(FakeObservationPort::default());
    assert!(port.publish(page()));
    let connections = Arc::new(ConnectionHooks::default());
    let client = ClientId::new("desktop").unwrap();
    let connection = connections.open(client.clone());
    let p = Provenance::human(Surface::Desktop, client.clone());
    let service = ObservationService::new(
        Some(port.clone()),
        DataPlane::default(),
        connections.clone(),
        7,
    );
    let ObservationResult::Subscribed { subscription, .. } = service
        .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
        .unwrap()
    else {
        panic!()
    };
    let query = BrowserQuery::GetAgentSubscription { subscription };
    for surface in [Surface::Web, Surface::Cli, Surface::None] {
        assert!(
            service
                .query(&query, &Provenance::human(surface, client.clone()))
                .is_err()
        );
    }
    let other = ClientId::new("other").unwrap();
    let _other = connections.open(other.clone());
    assert!(
        service
            .query(&query, &Provenance::human(Surface::Desktop, other))
            .is_err()
    );
    let mut old = scope();
    old.page_generation += 1;
    assert!(matches!(
        service
            .query(&BrowserQuery::GetAgentObservation { scope: old }, &p)
            .unwrap(),
        ObservationReply::State(ObservationState::Revoked)
    ));
    port.revoke(&scope());
    assert!(service.query(&query, &p).is_err());
    drop(connection);
    let _new = connections.open(client);
    assert!(service.query(&query, &p).is_err());
}
#[test]
fn coherent_publication_rejects_new_tab_with_old_pixels_and_absent_backend_is_closed() {
    let mut page = page();
    page.snapshot.active_tab = Some(id());
    assert!(!page.validate());
    let port = FakeObservationPort::default();
    assert!(!port.publish(page));
    let connections = Arc::new(ConnectionHooks::default());
    let client = ClientId::new("desktop").unwrap();
    let _connection = connections.open(client.clone());
    let service = ObservationService::new(None, DataPlane::default(), connections, 7);
    assert!(matches!(
        service
            .query(
                &BrowserQuery::GetAgentObservation { scope: scope() },
                &Provenance::human(Surface::Desktop, client)
            )
            .unwrap(),
        ObservationReply::State(ObservationState::Unavailable)
    ));
}

// Every fixture records only bounded payload-free notifications. recv() is the
// completion checkpoint; tests never infer completion from elapsed wall time.
struct Harness {
    service: Arc<ObservationService>,
    port: Arc<FakeObservationPort>,
    hooks: Arc<ConnectionHooks>,
    connection: Option<crate::connections::ClientConnection>,
    p: Provenance,
    live: Arc<std::sync::atomic::AtomicBool>,
    events: tokio::sync::mpsc::Receiver<ObservationEvent>,
}
impl Harness {
    fn new() -> Self {
        let port = Arc::new(FakeObservationPort::default());
        assert!(port.publish(page()));
        let hooks = Arc::new(ConnectionHooks::default());
        let client = ClientId::new("desktop").unwrap();
        let connection = Some(hooks.open(client.clone()));
        let p = Provenance::human(Surface::Desktop, client);
        let live = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let is_live = live.clone();
        let authority = Authority {
            target: Arc::new(|p, t| *p == scope().project && *t == scope().thread),
            scope: Arc::new(move |s| {
                if *s != scope() {
                    return Err(ObservationState::Revoked);
                }
                if !is_live.load(std::sync::atomic::Ordering::Acquire) {
                    return Err(ObservationState::Stopped);
                }
                Ok(())
            }),
        };
        let (tx, events) = tokio::sync::mpsc::channel(64);
        let service = ObservationService::build(
            Some(port.clone()),
            DataPlane::default(),
            hooks.clone(),
            7,
            authority,
            Arc::new(move |event| tx.try_send(event).unwrap()),
        );
        Self {
            service,
            port,
            hooks,
            connection,
            p,
            live,
            events,
        }
    }
    fn subscribe(&self) -> ObservationId {
        let ObservationResult::Subscribed { subscription, .. } = self
            .service
            .command(
                BrowserCommand::SubscribeAgentFrames { scope: scope() },
                &self.p,
            )
            .unwrap()
        else {
            panic!()
        };
        subscription
    }
    fn token(&self, subscription: &ObservationId) -> ObservationId {
        let ObservationReply::Subscription {
            token: Some(token), ..
        } = self
            .service
            .query(
                &BrowserQuery::GetAgentSubscription {
                    subscription: subscription.clone(),
                },
                &self.p,
            )
            .unwrap()
        else {
            panic!()
        };
        token
    }
    async fn bind(
        &self,
        subscription: &ObservationId,
    ) -> crate::areas::browser::renderer::data::AgentFrameLease {
        let token = self.token(subscription);
        let channel = self.service.plane.open_channel().unwrap();
        self.service
            .command(
                BrowserCommand::BindAgentData {
                    subscription: subscription.clone(),
                    token,
                    channel_id: channel.id().clone(),
                },
                &self.p,
            )
            .unwrap();
        let Lease::AgentFrames(lease) = channel.bound().await.unwrap() else {
            panic!()
        };
        lease
    }
}
#[tokio::test]
async fn snapshot_ahead_of_feed_does_not_falsely_revoke_subscription() {
    // A source may publish its new snapshot before waking an existing feed.
    // Its coherent old feed page is not a rollback from the separate snapshot.
    struct Ahead(Arc<FakeObservationPort>);
    impl AgentObservationPort for Ahead {
        fn list(
            &self,
            p: &ProjectId,
            t: &ThreadId,
        ) -> Result<Vec<ObservationScope>, ObservationFailure> {
            self.0.list(p, t)
        }
        fn snapshot(&self, s: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
            let mut latest = (*self.0.snapshot(s)?).clone();
            latest.snapshot.revision = page().snapshot.revision + 1;
            Ok(Arc::new(latest))
        }
        fn subscribe(&self, s: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
            self.0.subscribe(s)
        }
        fn current(&self, s: &ObservationScope) -> bool {
            self.0.current(s)
        }
    }
    let mut h = Harness::new();
    h.service = ObservationService::build(
        Some(Arc::new(Ahead(h.port.clone()))),
        h.service.plane.clone(),
        h.hooks.clone(),
        7,
        h.service.authority.clone(),
        h.service.publish.clone(),
    );
    let subscription = h.subscribe();
    assert_eq!(
        h.events.recv().await.unwrap().revision,
        page().snapshot.revision
    );
    h.service.prune();
    assert!(
        h.service
            .subscriptions
            .lock()
            .unwrap()
            .contains_key(&subscription)
    );
    assert!(h.events.try_recv().is_err());
    let mut next = page();
    next.snapshot.revision += 1;
    assert!(h.port.publish(next.clone()));
    h.service.prune();
    assert_eq!(
        h.events.recv().await.unwrap().revision,
        next.snapshot.revision
    );
    let mut lease = h.bind(&subscription).await;
    assert!(lease.next().await.is_some());
    assert!(lease.current());
}

#[tokio::test]
async fn terminal_run_rejects_new_subscription_and_revokes_existing_lease() {
    let mut h = Harness::new();
    let subscription = h.subscribe();
    let mut lease = h.bind(&subscription).await;
    assert!(lease.next().await.is_some());
    h.live.store(false, std::sync::atomic::Ordering::Release);
    // The fake source deliberately still says current=true after run end.
    assert!(h.port.current(&scope()));
    assert!(
        !lease.current(),
        "host live authority is checked at data transfer"
    );
    h.service.prune();
    assert!(lease.next().await.is_none());
    assert!(matches!(
        h.service
            .command(
                BrowserCommand::SubscribeAgentFrames { scope: scope() },
                &h.p
            )
            .unwrap(),
        ObservationResult::State(ObservationState::Stopped)
    ));
    assert_eq!(
        h.events.recv().await.unwrap().state,
        ObservationState::Starting
    );
    assert_eq!(h.events.recv().await.unwrap().state, ObservationState::Live);
    assert_eq!(
        h.events.recv().await.unwrap().state,
        ObservationState::Stopped
    );
    assert!(h.events.try_recv().is_err());
    assert!(h.service.subscriptions.lock().unwrap().is_empty());
}
#[tokio::test]
async fn updates_are_not_lost_to_query_bind_and_terminal_requests() {
    let mut h = Harness::new();
    let tasks = crate::areas::AreaTasks::new(tokio::runtime::Handle::current());
    let bus = event_bus::EventBus::new(16);
    h.service
        .start_lifecycle(&tasks, bus.subscribe(), Default::default())
        .unwrap();
    let subscription = h.subscribe();
    assert_eq!(
        h.events.recv().await.unwrap().state,
        ObservationState::Starting
    );
    let mut changed = page();
    changed.snapshot.revision = 2;
    changed.snapshot.tabs[0].url = "https://example.test/secret-2".into();
    assert!(h.port.publish(changed.clone()));
    // A competing query can consume the watch version; it must publish the same
    // revision, and the background watcher must not replay a stale Live later.
    let _ = h.token(&subscription);
    let event = h.events.recv().await.unwrap();
    assert_eq!(
        (event.revision, event.state),
        (2, ObservationState::Starting)
    );
    let _lease = h.bind(&subscription).await;
    assert_eq!(h.events.recv().await.unwrap().state, ObservationState::Live);
    changed.snapshot.revision = 3;
    assert!(h.port.publish(changed));
    let event = h.events.recv().await.unwrap();
    assert_eq!((event.revision, event.state), (3, ObservationState::Live));
    let json = serde_json::to_value(&event).unwrap();
    assert_eq!(json.as_object().unwrap().len(), 3);
    assert!(
        json.get("subscription").is_some()
            && json.get("revision").is_some()
            && json.get("state").is_some()
    );
    h.service
        .command(
            BrowserCommand::UnsubscribeAgentFrames { subscription },
            &h.p,
        )
        .unwrap();
    assert_eq!(
        h.events.recv().await.unwrap().state,
        ObservationState::Stopped
    );
    tasks.shutdown().await.unwrap();
    assert!(h.events.try_recv().is_err());
}
#[tokio::test]
async fn client_isolation_late_disconnect_and_shared_fanout_survive() {
    let mut h = Harness::new();
    let a = h.subscribe();
    let token = h.token(&a);
    let channel = h.service.plane.open_channel().unwrap();
    let other_client = ClientId::new("other").unwrap();
    let other_connection = h.hooks.open(other_client.clone());
    let other = Provenance::human(Surface::Desktop, other_client);
    let bind = BrowserCommand::BindAgentData {
        subscription: a.clone(),
        token: token.clone(),
        channel_id: channel.id().clone(),
    };
    for command in [
        bind.clone(),
        BrowserCommand::UnsubscribeAgentFrames {
            subscription: a.clone(),
        },
    ] {
        assert_eq!(
            h.service.command(command, &other).unwrap_err().code,
            ErrorCode::PermissionDenied
        );
    }
    let b = match h
        .service
        .command(
            BrowserCommand::SubscribeAgentFrames { scope: scope() },
            &other,
        )
        .unwrap()
    {
        ObservationResult::Subscribed { subscription, .. } => subscription,
        _ => panic!(),
    };
    let old_epoch = h.connection.as_ref().unwrap().epoch();
    drop(h.connection.take());
    assert!(h.service.command(bind, &h.p).is_err());
    h.connection = Some(h.hooks.open(h.p.client_id.clone().unwrap()));
    let new = h.subscribe();
    h.service
        .disconnect(h.p.client_id.as_ref().unwrap(), old_epoch);
    assert!(
        h.service
            .query(
                &BrowserQuery::GetAgentSubscription { subscription: new },
                &h.p
            )
            .is_ok()
    );
    assert!(
        h.service
            .query(
                &BrowserQuery::GetAgentSubscription { subscription: b },
                &other
            )
            .is_ok()
    );
    assert!(
        h.port.current(&scope()),
        "disconnect never closes the run's page"
    );
    drop(other_connection);
}
#[tokio::test]
async fn authorization_matrix_refuses_forged_scopes_and_non_native_actors() {
    let h = Harness::new();
    for index in 0..7 {
        let mut forged = scope();
        match index {
            0 => forged.project = ProjectId::new("other").unwrap(),
            1 => forged.thread = ThreadId::new("other").unwrap(),
            2 => forged.run = RunId::new("other").unwrap(),
            3 => forged.incarnation = RunId::new("other").unwrap(),
            4 => forged.core_epoch += 1,
            5 => forged.browser_generation += 1,
            _ => forged.page_generation += 1,
        }
        let mut forged_page = page();
        forged_page.snapshot.scope = forged.clone();
        forged_page.frame = None;
        assert!(h.port.publish(forged_page));
        assert!(!matches!(
            h.service.query(
                &BrowserQuery::GetAgentObservation {
                    scope: forged.clone()
                },
                &h.p
            ),
            Ok(ObservationReply::Snapshot(_))
        ));
        assert!(!matches!(
            h.service
                .command(BrowserCommand::SubscribeAgentFrames { scope: forged }, &h.p),
            Ok(ObservationResult::Subscribed { .. })
        ));
    }
    let ObservationReply::Runs { scopes } = h
        .service
        .query(
            &BrowserQuery::ListAgentRuns {
                project: scope().project,
                thread: scope().thread,
            },
            &h.p,
        )
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(scopes, [scope()]);
    for surface in [Surface::Web, Surface::Cli, Surface::None] {
        let mut p = h.p.clone();
        p.surface = surface;
        assert!(
            h.service
                .query(&BrowserQuery::GetAgentObservation { scope: scope() }, &p)
                .is_err()
        );
        assert!(
            h.service
                .command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p)
                .is_err()
        );
    }
    {
        let mut p = h.p.clone();
        p.actor = Actor::Agent;
        assert!(
            h.service
                .query(&BrowserQuery::GetAgentObservation { scope: scope() }, &p)
                .is_err()
        );
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Checkpoint {
    List,
    Snapshot,
    Subscribe,
}
struct CheckpointPort {
    inner: Arc<FakeObservationPort>,
    armed: Mutex<Option<Checkpoint>>,
    reached: std::sync::Barrier,
    resume: std::sync::Barrier,
}
impl CheckpointPort {
    fn pause(&self, point: Checkpoint) {
        let armed = self.armed.lock().unwrap().take();
        if armed == Some(point) {
            self.reached.wait();
            self.resume.wait();
        } else if armed.is_some() {
            *self.armed.lock().unwrap() = armed;
        }
    }
}
impl AgentObservationPort for CheckpointPort {
    fn list(
        &self,
        p: &ProjectId,
        t: &ThreadId,
    ) -> Result<Vec<ObservationScope>, ObservationFailure> {
        let result = self.inner.list(p, t);
        self.pause(Checkpoint::List);
        result
    }
    fn snapshot(&self, s: &ObservationScope) -> Result<Arc<ObservedPage>, ObservationFailure> {
        let result = self.inner.snapshot(s);
        self.pause(Checkpoint::Snapshot);
        result
    }
    fn subscribe(&self, s: &ObservationScope) -> Result<ObservationFeed, ObservationFailure> {
        let result = self.inner.subscribe(s);
        self.pause(Checkpoint::Subscribe);
        result
    }
    fn current(&self, s: &ObservationScope) -> bool {
        self.inner.current(s)
    }
}
#[test]
fn checkpoint_source_results_cannot_cross_revocation_or_disconnect() {
    // Bound scenarios and barriers select the interleaving precisely: a source
    // has captured a result, then authorization changes before it returns.
    for point in [
        Checkpoint::List,
        Checkpoint::Snapshot,
        Checkpoint::Subscribe,
    ] {
        for disconnect in [false, true] {
            let mut h = Harness::new();
            let checkpoint = Arc::new(CheckpointPort {
                inner: h.port.clone(),
                armed: Mutex::new(Some(point)),
                reached: std::sync::Barrier::new(2),
                resume: std::sync::Barrier::new(2),
            });
            let service = ObservationService::build(
                Some(checkpoint.clone()),
                h.service.plane.clone(),
                h.hooks.clone(),
                7,
                h.service.authority.clone(),
                Arc::new(|_| {}),
            );
            let p = h.p.clone();
            std::thread::scope(|threads| {
                let request = threads.spawn(|| match point {
                    Checkpoint::List => service.query(&BrowserQuery::ListAgentRuns { project: scope().project, thread: scope().thread }, &p).map(|reply| matches!(reply, ObservationReply::Runs { scopes } if !scopes.is_empty())),
                    Checkpoint::Snapshot => service.query(&BrowserQuery::GetAgentObservation { scope: scope() }, &p).map(|reply| matches!(reply, ObservationReply::Snapshot(_))),
                    Checkpoint::Subscribe => service.command(BrowserCommand::SubscribeAgentFrames { scope: scope() }, &p).map(|reply| matches!(reply, ObservationResult::Subscribed { .. })),
                });
                checkpoint.reached.wait();
                if disconnect {
                    drop(h.connection.take());
                } else {
                    h.port.revoke(&scope());
                }
                checkpoint.resume.wait();
                assert!(!request.join().unwrap().unwrap_or(false));
            });
            assert!(service.subscriptions.lock().unwrap().is_empty());
            let slots = (0..8)
                .map(|_| service.plane.frame_slot().unwrap())
                .collect::<Vec<_>>();
            drop(slots);
        }
    }
}
#[tokio::test]
async fn checkpoint_bind_after_stop_never_authorizes_an_old_payload() {
    let h = Harness::new();
    let checkpoint = Arc::new(CheckpointPort {
        inner: h.port.clone(),
        armed: Mutex::new(None),
        reached: std::sync::Barrier::new(2),
        resume: std::sync::Barrier::new(2),
    });
    let service = ObservationService::build(
        Some(checkpoint.clone()),
        h.service.plane.clone(),
        h.hooks.clone(),
        7,
        h.service.authority.clone(),
        Arc::new(|_| {}),
    );
    let ObservationResult::Subscribed { subscription, .. } = service
        .command(
            BrowserCommand::SubscribeAgentFrames { scope: scope() },
            &h.p,
        )
        .unwrap()
    else {
        panic!()
    };
    let ObservationReply::Subscription {
        token: Some(token), ..
    } = service
        .query(
            &BrowserQuery::GetAgentSubscription {
                subscription: subscription.clone(),
            },
            &h.p,
        )
        .unwrap()
    else {
        panic!()
    };
    let channel = service.plane.open_channel().unwrap();
    *checkpoint.armed.lock().unwrap() = Some(Checkpoint::Snapshot);
    std::thread::scope(|threads| {
        let request = threads.spawn(|| {
            service.command(
                BrowserCommand::BindAgentData {
                    subscription,
                    token,
                    channel_id: channel.id().clone(),
                },
                &h.p,
            )
        });
        checkpoint.reached.wait();
        h.live.store(false, std::sync::atomic::Ordering::Release);
        checkpoint.resume.wait();
        assert!(request.join().unwrap().is_err());
    });
    assert!(service.plane.is_pending(channel.id()));
    service.prune();
    assert!(service.subscriptions.lock().unwrap().is_empty());
}

struct ControlledModel {
    entered: Notify,
    finish: Notify,
    fail: bool,
}
#[async_trait::async_trait]
impl runtime::AgentModel for ControlledModel {
    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "observation-test".into()
    }
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        self.entered.notify_one();
        self.finish.notified().await;
        if self.fail {
            return Err(runtime::RuntimeError::Model {
                reason: "test failure".into(),
            });
        }
        Ok(providers::ChatResponse {
            message: providers::Message {
                role: providers::Role::Assistant,
                content: vec![providers::ContentBlock::Text {
                    text: "done".into(),
                }],
            },
            finish_reason: providers::FinishReason::Stop,
            usage: Default::default(),
            usage_reported: true,
        })
    }
}
struct RuntimeHarness {
    h: Harness,
    runtime: runtime::AgentRuntime,
    model: Arc<ControlledModel>,
    run: runtime::RunId,
    scope: ObservationScope,
    store: storage::Storage,
    sidebar: workspace_state::SidebarStore,
    _dir: tempfile::TempDir,
}
impl RuntimeHarness {
    async fn new(fail: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = storage::StorageConfig {
            db_path: dir.path().join("host.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(config.clone()).unwrap();
        let bus = Arc::new(event_bus::EventBus::new(64));
        let model = Arc::new(ControlledModel {
            entered: Notify::new(),
            finish: Notify::new(),
            fail,
        });
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&config, store.handle()).unwrap());
        let run = runtime.delegate_background(
            runtime::Role::Worker,
            "observe".into(),
            Default::default(),
        );
        model.entered.notified().await;
        let mut scope = scope();
        scope.run = RunId::new(run.to_string()).unwrap();
        scope.incarnation = RunId::new(
            runtime
                .browser_execution_reader()
                .observe(run)
                .unwrap()
                .incarnation,
        )
        .unwrap();
        store
            .handle()
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: scope.thread.to_string(),
                project_id: Some(scope.project.to_string()),
                project_root: Some(dir.path().to_string_lossy().into_owned()),
                root_run_id: Some(run.to_string()),
                ..Default::default()
            }])
            .unwrap();
        let sidebar = workspace_state::SidebarStore::new(dir.path().join("sidebar.json"));
        let mut document = workspace_state::SidebarDocument::default();
        document
            .sidebar
            .add_project(
                workspace_state::ProjectId::new(scope.project.to_string()),
                "Project",
                dir.path(),
            )
            .unwrap();
        document.sidebar.projects[0].id =
            workspace_state::ProjectId::new(scope.project.to_string());
        let lock = sidebar.lock().unwrap();
        sidebar.write(&lock, &document).unwrap();
        drop(lock);
        let mut h = Harness::new();
        let mut page = page();
        page.snapshot.scope = scope.clone();
        page.frame = None;
        assert!(h.port.publish(page));
        let (tx, events) = tokio::sync::mpsc::channel(64);
        h.events = events;
        h.service = ObservationService::build(
            Some(h.port.clone()),
            DataPlane::default(),
            h.hooks.clone(),
            7,
            Authority::runtime(
                runtime.downgrade(),
                sidebar.path().into(),
                Default::default(),
            ),
            Arc::new(move |event| tx.try_send(event).unwrap()),
        );
        Self {
            h,
            runtime,
            model,
            run,
            scope,
            store,
            sidebar,
            _dir: dir,
        }
    }
    fn subscribe(&self) -> ObservationId {
        let ObservationResult::Subscribed { subscription, .. } = self
            .h
            .service
            .command(
                BrowserCommand::SubscribeAgentFrames {
                    scope: self.scope.clone(),
                },
                &self.h.p,
            )
            .unwrap()
        else {
            panic!()
        };
        subscription
    }
}
#[tokio::test]
async fn stored_membership_and_runtime_terminal_phases_override_current_incarnation() {
    for phase in [
        event_bus::AgentRunPhase::Done,
        event_bus::AgentRunPhase::Error,
        event_bus::AgentRunPhase::Stopped,
    ] {
        let mut f = RuntimeHarness::new(phase == event_bus::AgentRunPhase::Error).await;
        let subscription = f.subscribe();
        let mut lease = f.h.bind(&subscription).await;
        assert!(lease.current());
        if phase == event_bus::AgentRunPhase::Stopped {
            f.runtime.stop(f.run, runtime::StopScope::SelfOnly).unwrap();
        } else {
            f.model.finish.notify_one();
        }
        // Stop must refuse data even before the terminal phase is published.
        if phase == event_bus::AgentRunPhase::Stopped {
            assert!(!lease.current());
        }
        assert_eq!(f.runtime.wait(f.run).await.unwrap(), phase);
        assert!(
            f.runtime
                .browser_execution_reader()
                .observe(f.run)
                .unwrap()
                .current()
        );
        assert!(f.h.port.current(&f.scope));
        assert!(!lease.current());
        f.h.service.prune();
        assert!(lease.next().await.is_none());
        assert!(matches!(
            f.h.service
                .command(
                    BrowserCommand::SubscribeAgentFrames {
                        scope: f.scope.clone()
                    },
                    &f.h.p
                )
                .unwrap(),
            ObservationResult::State(ObservationState::Stopped)
        ));
        assert_eq!(
            f.h.events.recv().await.unwrap().state,
            ObservationState::Starting
        );
        assert_eq!(
            f.h.events.recv().await.unwrap().state,
            ObservationState::Live
        );
        assert_eq!(
            f.h.events.recv().await.unwrap().state,
            ObservationState::Stopped
        );
    }
}
#[tokio::test]
async fn persisted_project_removal_or_thread_reaffiliation_revokes_live_pages() {
    for remove_project in [false, true] {
        let f = RuntimeHarness::new(false).await;
        let subscription = f.subscribe();
        let lease = f.h.bind(&subscription).await;
        if remove_project {
            let lock = f.sidebar.lock().unwrap();
            f.sidebar
                .write(&lock, &workspace_state::SidebarDocument::default())
                .unwrap();
        } else {
            f.store
                .handle()
                .upsert_threads_host(&[storage::threads::ThreadUpsert {
                    thread_id: f.scope.thread.to_string(),
                    project_id: Some("other-project".into()),
                    ..Default::default()
                }])
                .unwrap();
        }
        assert!(!lease.current());
        assert!(
            f.h.service
                .query(&BrowserQuery::GetAgentSubscription { subscription }, &f.h.p)
                .is_err()
        );
        assert!(
            f.h.service
                .query(
                    &BrowserQuery::ListAgentRuns {
                        project: f.scope.project.clone(),
                        thread: f.scope.thread.clone()
                    },
                    &f.h.p
                )
                .is_err()
        );
        f.runtime.cancel(f.run).unwrap();
        f.runtime.wait(f.run).await.unwrap();
    }
}

#[tokio::test]
async fn replayed_notification_id_is_not_a_capability_for_other_native_or_web_clients() {
    let mut h = Harness::new();
    let subscription = h.subscribe();
    let public = h.events.recv().await.unwrap();
    assert_eq!(public.subscription, subscription);
    let stolen_token = h.token(&subscription);
    let other = ClientId::new("notification-reader").unwrap();
    let _connection = h.hooks.open(other.clone());
    for surface in [Surface::Desktop, Surface::Web] {
        let p = Provenance::human(surface, other.clone());
        let channel = h.service.plane.open_channel().unwrap();
        for token in [id(), stolen_token.clone()] {
            assert_eq!(
                h.service
                    .command(
                        BrowserCommand::BindAgentData {
                            subscription: public.subscription.clone(),
                            token,
                            channel_id: channel.id().clone()
                        },
                        &p
                    )
                    .unwrap_err()
                    .code,
                ErrorCode::PermissionDenied
            );
        }
        assert_eq!(
            h.service
                .query(
                    &BrowserQuery::GetAgentSubscription {
                        subscription: public.subscription.clone()
                    },
                    &p
                )
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
        assert_eq!(
            h.service
                .command(
                    BrowserCommand::UnsubscribeAgentFrames {
                        subscription: public.subscription.clone()
                    },
                    &p
                )
                .unwrap_err()
                .code,
            ErrorCode::PermissionDenied
        );
        assert!(h.service.plane.is_pending(channel.id()));
    }
    // Failed attacks neither consume the owner's token nor cancel its lease.
    let mut owner = h.bind(&subscription).await;
    assert!(owner.next().await.is_some());
}
#[tokio::test]
async fn source_revocation_restart_and_shutdown_notify_and_release_without_page_close() {
    use crate::bootstrap::restart::{HostRestartGate, HostRestartPhase};
    for end in 0..3 {
        let mut h = Harness::new();
        let tasks = crate::areas::AreaTasks::new(tokio::runtime::Handle::current());
        let bus = event_bus::EventBus::new(16);
        let gate = HostRestartGate::default();
        h.service
            .start_lifecycle(&tasks, bus.subscribe(), gate.clone())
            .unwrap();
        let subscription = h.subscribe();
        let lease = h.bind(&subscription).await;
        assert_eq!(
            h.events.recv().await.unwrap().state,
            ObservationState::Starting
        );
        assert_eq!(h.events.recv().await.unwrap().state, ObservationState::Live);
        match end {
            0 => h.port.revoke(&scope()),
            1 => {
                assert!(gate.transition(1, HostRestartPhase::Preparing));
            }
            _ => tasks.request_stop(),
        }
        assert_eq!(
            h.events.recv().await.unwrap().state,
            ObservationState::Revoked
        );
        assert!(!lease.current());
        assert!(h.service.subscriptions.lock().unwrap().is_empty());
        if end != 0 {
            assert!(
                h.port.current(&scope()),
                "host cleanup never closes the page"
            );
        }
        tasks.shutdown().await.unwrap();
        drop(lease);
        let slots = (0..8)
            .map(|_| h.service.plane.frame_slot().unwrap())
            .collect::<Vec<_>>();
        drop(slots);
    }
}
#[tokio::test(start_paused = true)]
async fn bind_window_is_exact_and_bound_stream_is_not_an_unbound_timeout() {
    let h = Harness::new();
    let subscription = h.subscribe();
    tokio::time::advance(std::time::Duration::from_millis(29_999)).await;
    let lease = h.bind(&subscription).await;
    tokio::time::advance(std::time::Duration::from_millis(1)).await;
    h.service.prune();
    assert!(lease.current());
    drop(lease); // Slow/disconnected data reader is pruned without touching page.
    h.service.prune();
    assert!(h.service.subscriptions.lock().unwrap().is_empty());
    assert!(h.port.current(&scope()));
}

#[tokio::test]
async fn thread_deletion_tombstone_revokes_before_storage_reclaim() {
    let f = RuntimeHarness::new(false).await;
    let subscription = f.subscribe();
    let lease = f.h.bind(&subscription).await;
    let connection = rusqlite::Connection::open(f._dir.path().join("host.db")).unwrap();
    connection.execute("INSERT INTO thread_tombstones(thread_id, family_key, deleted_at_ns) VALUES (?1, 'test-family', 1)", [f.scope.thread.as_str()]).unwrap();
    assert!(f.h.port.current(&f.scope));
    assert!(!lease.current());
    f.h.service.prune();
    assert!(f.h.service.subscriptions.lock().unwrap().is_empty());
    assert!(
        f.h.service
            .query(
                &BrowserQuery::ListAgentRuns {
                    project: f.scope.project.clone(),
                    thread: f.scope.thread.clone()
                },
                &f.h.p
            )
            .is_err()
    );
    f.runtime.cancel(f.run).unwrap();
    f.runtime.wait(f.run).await.unwrap();
}
