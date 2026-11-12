use gui::core_client::{Draft, Shared, cache::Cache, client_config, connection::snapshot};
use workbench_client::{
    WorkbenchClient,
    fake::{FakeServer, FakeTransport},
};
use workbench_protocol::namespaces::{
    ApprovalsEvent, ProjectionQuery, ProjectionReply, WorkspaceEvent,
};
use workbench_protocol::{
    envelope::*, events::*, handshake::*, ids::*, queries::*, views::*, wire::*,
};

fn cursor(epoch: u64, sequence: u64) -> EventCursor {
    EventCursor {
        core_epoch: InstanceEpoch(epoch),
        sequence: Sequence(sequence),
    }
}
fn thread(title: &str, revision: u64) -> ThreadSummary {
    ThreadSummary {
        id: ThreadId::new("t").unwrap(),
        project: ProjectId::new("p").unwrap(),
        title: title.into(),
        state: ThreadState::Idle,
        lead_thread: None,
        revision: Revision(revision),
        updated_at: UnixMillis(0),
    }
}
fn event(epoch: u64, sequence: u64, title: &str, revision: u64) -> EventEnvelope {
    EventEnvelope {
        subscription_id: SubscriptionId::new("s").unwrap(),
        cursor: cursor(epoch, sequence),
        caused_by_request_id: None,
        event: UiEvent::Workspace(WorkspaceEvent::ThreadChanged {
            thread: thread(title, revision),
        }),
    }
}
async fn connected() -> (WorkbenchClient, FakeServer) {
    let (transport, mut server) = FakeTransport::pair();
    let config = client_config();
    let hello = ServerHello {
        protocol_version: ProtocolVersion::CURRENT,
        capabilities: Capability::IMPLEMENTED.to_vec(),
        server_build: config.client_build.clone(),
        core: CoreIdentity::new("core").unwrap(),
        instance_epoch: InstanceEpoch(7),
        limits: WireLimits::default(),
        status: CoreStatus {
            state: CoreState::Ready,
            running: Default::default(),
            retention_reasons: vec![],
        },
    };
    let (client, _) = tokio::join!(
        WorkbenchClient::connect(transport, config),
        server.handshake(&hello)
    );
    (client.unwrap(), server)
}
async fn subscription(
    client: &WorkbenchClient,
    server: &mut FakeServer,
) -> workbench_client::Subscription {
    let request = client.subscribe(Subscribe {
        scope: SubscriptionScope::All,
        after: None,
    });
    let response = async {
        let request = server.recv_request().await.unwrap();
        server.reply(
            &request.request_id,
            ReplyResult::Success(ReplyPayload::SubscriptionStarted(SubscriptionStarted {
                subscription_id: SubscriptionId::new("s").unwrap(),
                snapshot_id: Some(SnapshotId::new("cut").unwrap()),
                high_water: cursor(7, 40),
                initial_page: Some(Page {
                    items: vec![SnapshotItem::Thread(thread("old", 999))],
                    next_cursor: Some(PageCursor::new("next").unwrap()),
                    snapshot_revision: Revision(40),
                }),
            })),
        );
    };
    let (sub, _) = tokio::join!(request, response);
    sub.unwrap()
}

#[tokio::test]
async fn paged_snapshot_keeps_its_cut_then_applies_later_events_not_revision_order() {
    let (client, mut server) = connected().await;
    let mut subscription = subscription(&client, &mut server).await;
    let expected = cursor(7, 40);
    let reader = snapshot(&client, &mut subscription);
    let responder = async {
        let page = server.recv_request().await.unwrap();
        assert!(matches!(
            page.body,
            RequestBody::Query(Query::Projection(ProjectionQuery::ReadSnapshot(_)))
        ));
        server.event(event(7, 39, "before", 1000));
        server.event(event(7, 41, "new", 1));
        server.event(event(7, 41, "duplicate", 500));
        server.reply(
            &page.request_id,
            ReplyResult::Success(ReplyPayload::Query(QueryReply::Projection(
                ProjectionReply::Snapshot(SnapshotPage {
                    snapshot_id: SnapshotId::new("cut").unwrap(),
                    high_water: expected,
                    page: Page {
                        items: vec![SnapshotItem::Thread(thread("snapshot", 999))],
                        next_cursor: None,
                        snapshot_revision: Revision(40),
                    },
                }),
            ))),
        );
    };
    let (cache, _) = tokio::join!(reader, responder);
    let mut cache = cache.unwrap();
    // Events not consumed during paging remain in the same bounded subscription.
    while cache.threads["t"].title != "new" {
        assert!(cache.event(subscription.next().await.unwrap()));
    }
    assert_eq!(cache.threads["t"].revision, Revision(1));
    assert_eq!(cache.cursor(), Some(cursor(7, 41)));
}

#[tokio::test]
async fn overflow_or_epoch_change_requires_new_snapshot_instead_of_mixing_cuts() {
    for change_epoch in [false, true] {
        let (client, mut server) = connected().await;
        let mut subscription = subscription(&client, &mut server).await;
        let reader = snapshot(&client, &mut subscription);
        let sender = async {
            let page = server.recv_request().await.unwrap();
            if change_epoch {
                server.event(event(8, 1, "restart", 0));
            } else {
                for n in 41..=600 {
                    server.event(event(7, n, "busy", n));
                }
            }
            page
        };
        let (cache, old_page) = tokio::join!(reader, sender);
        assert!(cache.is_err());
        if !change_epoch {
            // A late reply for the abandoned page cannot revive the old cut.
            server.reply(
                &old_page.request_id,
                ReplyResult::Success(ReplyPayload::Query(QueryReply::Projection(
                    ProjectionReply::Snapshot(SnapshotPage {
                        snapshot_id: SnapshotId::new("cut").unwrap(),
                        high_water: cursor(7, 40),
                        page: Page {
                            items: vec![],
                            next_cursor: None,
                            snapshot_revision: Revision(40),
                        },
                    }),
                ))),
            );
            let request = client.subscribe(Subscribe {
                scope: SubscriptionScope::All,
                after: None,
            });
            let respond = async {
                let request = server.recv_request().await.unwrap();
                assert!(matches!(
                    request.body,
                    RequestBody::Control(workbench_protocol::envelope::Control::Subscribe(_))
                ));
                server.reply(
                    &request.request_id,
                    ReplyResult::Success(ReplyPayload::SubscriptionStarted(SubscriptionStarted {
                        subscription_id: SubscriptionId::new("resynced").unwrap(),
                        snapshot_id: Some(SnapshotId::new("new-cut").unwrap()),
                        high_water: cursor(7, 1000),
                        initial_page: Some(Page {
                            items: vec![SnapshotItem::Thread(thread("replacement", 1))],
                            next_cursor: None,
                            snapshot_revision: Revision(1),
                        }),
                    })),
                );
            };
            let (fresh, _) = tokio::join!(request, respond);
            let mut fresh = fresh.unwrap();
            let cache = snapshot(&client, &mut fresh).await.unwrap();
            assert_eq!(cache.threads["t"].title, "replacement");
            assert_eq!(cache.cursor(), Some(cursor(7, 1000)));
        }
    }
}

#[test]
fn epoch_replacement_keeps_human_input_and_deduplicates_thread_events() {
    let mut shared = Shared::default();
    shared.drafts.insert(
        "t".into(),
        Draft {
            text: "unsent".into(),
            revision: 5,
        },
    );
    shared.image_drafts.insert(
        "t".into(),
        vec![gui::core_client::ImageDraft {
            id: uuid::Uuid::new_v4(),
            uploading: false,
            mime: "image/png".into(),
            data: "retained bytes".into(),
            reference: Some(workbench_protocol::commands::ImageRef {
                attachment_id: AttachmentId::new("validated").unwrap(),
                mime_type: "image/png".into(),
            }),
        }],
    );
    shared.outcome_unknown.insert("pending-request".into());
    shared.cache.at(cursor(7, 40));
    shared.replace_projection(shared.cache.clone());
    assert!(shared.image_drafts["t"][0].reference.is_some());
    assert!(!shared.cache.event(event(8, 1, "other epoch", 0)));
    let mut fresh = Cache::default();
    fresh.at(cursor(8, 0));
    fresh.snapshot(SnapshotItem::Thread(thread("fresh", 1)));
    shared.replace_projection(fresh);
    assert_eq!(shared.drafts["t"].text, "unsent");
    assert_eq!(shared.image_drafts["t"][0].data, "retained bytes");
    assert!(shared.image_drafts["t"][0].reference.is_none());
    assert!(shared.outcome_unknown.contains("pending-request"));
    assert!(shared.cache.event(event(8, 1, "new", 0)));
    assert!(shared.cache.event(event(8, 1, "duplicate", 100)));
    assert_eq!(shared.cache.threads["t"].title, "new");
}

#[test]
fn older_history_prepends_but_refresh_replaces_without_reordering_old_pages() {
    let message = |index| CommittedMessage {
        thread: ThreadId::new("t").unwrap(),
        run: RunId::new("r").unwrap(),
        index: Sequence(index),
        role: MessageRole::User,
        content: MessageContent::Inline {
            text: index.to_string(),
        },
    };
    let page = |items| Page {
        items,
        next_cursor: None,
        snapshot_revision: Revision(0),
    };
    let mut cache = Cache::default();
    cache.history_page("t", page(vec![message(2), message(3)]), false);
    cache.history_page("t", page(vec![message(0), message(1), message(2)]), true);
    assert_eq!(
        cache.history["t"]
            .iter()
            .map(|m| m.index.get())
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3]
    );
    cache.history_page("t", page(vec![message(3), message(4)]), false);
    assert_eq!(
        cache.history["t"]
            .iter()
            .map(|m| m.index.get())
            .collect::<Vec<_>>(),
        vec![3, 4]
    );
}

#[test]
fn changed_approval_invalidates_the_input_previously_confirmed_by_the_human() {
    let approval = ApprovalView {
        id: ApprovalId::new("a").unwrap(),
        core_epoch: InstanceEpoch(7),
        revision: Revision(2),
        thread: ThreadId::new("t").unwrap(),
        run: RunId::new("r").unwrap(),
        tool_name: "shell".into(),
        summary: "new input".into(),
        summary_truncated: false,
        input_bytes: ByteOffset(9),
        state: ApprovalState::Pending,
        decided_by: None,
    };
    let mut cache = Cache::default();
    cache.at(cursor(7, 40));
    cache
        .approval_inputs
        .insert("a".into(), "previous input".into());
    assert!(cache.event(EventEnvelope {
        subscription_id: SubscriptionId::new("s").unwrap(),
        cursor: cursor(7, 41),
        caused_by_request_id: None,
        event: UiEvent::Approvals(ApprovalsEvent::ApprovalChanged { approval })
    }));
    assert!(!cache.approval_inputs.contains_key("a"));
    assert_eq!(cache.approvals["a"].revision, Revision(2));
}
