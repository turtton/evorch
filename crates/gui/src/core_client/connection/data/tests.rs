use super::*;
use crate::core_client::{cache::Cache, features::Feature};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{UnixListener, UnixStream},
    sync::mpsc,
};
use workbench_protocol::{
    commands::CommandReply,
    envelope::{ReplyPayload, ReplyResult, RequestBody},
    namespaces::browser::{BrowserQuery, RendererId, RendererTabRef, data::*},
    queries::Query,
};

fn id() -> RendererId {
    RendererId::new("a".repeat(32)).unwrap()
}
fn source() -> DataSource {
    DataSource::Frames {
        tab: RendererTabRef {
            core_epoch: 7,
            generation: 2,
            id: id(),
        },
        subscription: id(),
        token: id(),
    }
}
fn action(shared: &Arc<Mutex<Shared>>, source: DataSource) -> DataAction {
    DataAction {
        feature: FeatureId::Browser,
        tag: 1,
        generation: shared
            .lock()
            .unwrap()
            .features
            .data
            .generation(FeatureId::Browser),
        request: DataRequest::Start(source),
    }
}
struct Probe(mpsc::UnboundedSender<DataNotice>);
impl Feature for Probe {
    fn id(&self) -> FeatureId {
        FeatureId::Browser
    }
    fn title(&self) -> &'static str {
        "data probe"
    }
    fn data(&mut self, _tag: u64, notice: DataNotice) {
        let _ = self.0.send(notice);
    }
}
fn shared() -> (Arc<Mutex<Shared>>, mpsc::UnboundedReceiver<DataNotice>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut shared = Shared {
        connected: true,
        ..Shared::default()
    };
    shared.features.install(Box::new(Probe(tx)));
    (Arc::new(Mutex::new(shared)), rx)
}
async fn notice(rx: &mut mpsc::UnboundedReceiver<DataNotice>, expected: DataNotice) {
    loop {
        if rx.recv().await.unwrap() == expected {
            break;
        }
    }
}
async fn offer(listener: &UnixListener) -> UnixStream {
    let (mut peer, _) = listener.accept().await.unwrap();
    let mut hello = vec![0; encode_hello().len()];
    peer.read_exact(&mut hello).await.unwrap();
    assert_eq!(hello, encode_hello());
    peer.write_all(
        &ChannelOffer {
            channel: id(),
            bind_window_ms: BIND_WINDOW_MS,
        }
        .encode(),
    )
    .await
    .unwrap();
    peer
}
async fn bound(core: &mut workbench_client::fake::FakeServer) {
    let request = core.recv_request().await.unwrap();
    assert!(matches!(request.body, RequestBody::Command(_)));
    core.reply(
        &request.request_id,
        ReplyResult::Success(ReplyPayload::Command(CommandReply::accepted())),
    );
}
async fn eof(peer: &mut UnixStream) {
    let mut bytes = Vec::new();
    match peer.read_to_end(&mut bytes).await {
        Ok(_) => assert!(bytes.is_empty()),
        Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::ConnectionReset),
    }
}
async fn frame(peer: &mut UnixStream, sequence: u64, epoch: u64) {
    let start = FrameStart {
        core_epoch: epoch,
        generation: 2,
        tab: id(),
        sequence,
        width: 1,
        height: 1,
        stride: 4,
        total_bytes: 4,
    };
    peer.write_all(&start.encode()).await.unwrap();
    peer.write_all(
        &ChunkMeta {
            id: sequence,
            offset: 0,
        }
        .encode(&[1, 2, 3, 4]),
    )
    .await
    .unwrap();
    peer.write_all(
        &EndMeta {
            id: sequence,
            total_bytes: 4,
        }
        .encode(),
    )
    .await
    .unwrap();
    let mut ack = vec![0; AckMeta { id: sequence }.encode().len()];
    peer.read_exact(&mut ack).await.unwrap();
    assert_eq!(ack, AckMeta { id: sequence }.encode());
}
fn setup_listener() -> (tempfile::TempDir, UnixListener, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("core.sock");
    let listener = UnixListener::bind(&path).unwrap();
    (dir, listener, path)
}

#[tokio::test]
async fn disconnect_during_bind_closes_the_data_socket_and_empties_the_slot() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, _) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut peer = offer(&listener).await;
    let request = core.recv_request().await.unwrap();
    assert!(matches!(request.body, RequestBody::Command(_)));
    core.close();
    client.closed().await;
    readers
        .jobs
        .remove(&(FeatureId::Browser, 1))
        .unwrap()
        .task
        .await
        .unwrap();
    eof(&mut peer).await;
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    // Requests made while offline neither open nor bind another socket.
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    assert!(readers.jobs.is_empty());
    readers.stop(&shared, &ctx).await;
}

#[tokio::test]
async fn cancelling_a_pending_bind_joins_before_reselecting_an_endpoint_and_ignores_late_reply() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    let old = action(&shared, source());
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut peer = offer(&listener).await;
    let request = core.recv_request().await.unwrap();
    readers.stop(&shared, &ctx).await;
    eof(&mut peer).await; // observable server-side socket reclamation, no sleep
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    assert!(readers.jobs.is_empty());
    core.reply(
        &request.request_id,
        ReplyResult::Success(ReplyPayload::Command(CommandReply::accepted())),
    );
    // A request queued before reset cannot bind on the newly selected endpoint.
    readers.handle(old, &path, &client, &shared, &ctx).await;
    assert!(readers.jobs.is_empty());
    let (_new_dir, new_listener, new_path) = setup_listener();
    readers
        .handle(action(&shared, source()), &new_path, &client, &shared, &ctx)
        .await;
    let mut new_peer = offer(&new_listener).await;
    bound(&mut core).await;
    frame(&mut new_peer, 1, 7).await;
    notice(&mut notices, DataNotice::Ready).await;
    assert_eq!(shared.lock().unwrap().features.data.usage().1, 4);
    readers.stop(&shared, &ctx).await;
    eof(&mut new_peer).await;
}

#[tokio::test]
async fn reset_discards_a_frame_that_finishes_late_and_never_signals_ready() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut peer = offer(&listener).await;
    bound(&mut core).await;
    notice(&mut notices, DataNotice::Started).await;
    shared
        .lock()
        .unwrap()
        .begin_session(Cache::default(), vec![], &[], vec![]);
    frame(&mut peer, 1, 7).await;
    readers
        .jobs
        .remove(&(FeatureId::Browser, 1))
        .unwrap()
        .task
        .await
        .unwrap();
    eof(&mut peer).await;
    assert!(notices.try_recv().is_err());
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
}

#[tokio::test]
async fn data_epoch_must_match_control_and_latest_frames_do_not_accumulate() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut peer = offer(&listener).await;
    bound(&mut core).await;
    for sequence in 1..=5 {
        frame(&mut peer, sequence, 7).await;
        notice(&mut notices, DataNotice::Ready).await;
        assert_eq!(shared.lock().unwrap().features.data.usage().0, 1);
        assert_eq!(shared.lock().unwrap().features.data.usage().1, 4);
    }
    peer.write_all(
        &FrameStart {
            core_epoch: 8,
            generation: 2,
            tab: id(),
            sequence: 6,
            width: 1,
            height: 1,
            stride: 4,
            total_bytes: 4,
        }
        .encode(),
    )
    .await
    .unwrap();
    readers
        .jobs
        .remove(&(FeatureId::Browser, 1))
        .unwrap()
        .task
        .await
        .unwrap();
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    assert!(matches!(
        notices.recv().await.unwrap(),
        DataNotice::Failed(_)
    ));
    // A later control request fences the wire: no mutation replay or rebind preceded it.
    let query = client.query(Query::Browser(BrowserQuery::GetRendererStatus {}));
    let (_, ()) = tokio::join!(query, async {
        let request = core.recv_request().await.unwrap();
        assert!(matches!(request.body, RequestBody::Query(_)));
        core.close();
    });
}

#[tokio::test]
async fn blob_completes_in_a_slot_and_old_epoch_header_is_rejected_without_reading_body() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    for epoch in [7, 8] {
        readers
            .handle(
                action(
                    &shared,
                    DataSource::Capture {
                        operation_id: id(),
                        capture_id: id(),
                    },
                ),
                &path,
                &client,
                &shared,
                &ctx,
            )
            .await;
        let mut peer = offer(&listener).await;
        bound(&mut core).await;
        peer.write_all(
            &BlobStart {
                core_epoch: epoch,
                transfer_id: 9,
                source: BlobSource::Capture,
                media_type: MediaType::Png,
                total_bytes: 4,
                digest: [0; 32],
            }
            .encode(),
        )
        .await
        .unwrap();
        if epoch == 7 {
            peer.write_all(&ChunkMeta { id: 9, offset: 0 }.encode(&[1, 2, 3, 4]))
                .await
                .unwrap();
            peer.write_all(
                &EndMeta {
                    id: 9,
                    total_bytes: 4,
                }
                .encode(),
            )
            .await
            .unwrap();
            notice(&mut notices, DataNotice::Completed).await;
            assert!(
                matches!(shared.lock().unwrap().features.data.take(FeatureId::Browser, 1), Some(DataPayload::Blob(Blob { bytes, .. })) if bytes == [1,2,3,4])
            );
        }
        readers
            .jobs
            .remove(&(FeatureId::Browser, 1))
            .unwrap()
            .task
            .await
            .unwrap();
        if epoch == 7 {
            // The completed slot outlives its reaped task and decoded pixels may
            // still be displayed by the feature. Disconnect cleanup must signal it.
            assert!(readers.jobs.is_empty());
            readers.stop(&shared, &ctx).await;
            assert_eq!(notices.try_recv().unwrap(), DataNotice::Cancelled);
            assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
        }
    }
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    readers.stop(&shared, &ctx).await;
}

#[tokio::test]
async fn new_control_epoch_refuses_old_tab_and_requires_an_explicit_new_request() {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect_epoch(8).await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    let old = action(&shared, source());
    shared
        .lock()
        .unwrap()
        .begin_session(Cache::default(), vec![], &[], vec![]);
    readers.handle(old, &path, &client, &shared, &ctx).await;
    assert!(readers.jobs.is_empty());
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    assert!(readers.jobs.is_empty());
    assert!(matches!(
        notices.recv().await.unwrap(),
        DataNotice::Failed(_)
    ));
    let DataSource::Frames {
        mut tab,
        subscription,
        token,
    } = source()
    else {
        unreachable!()
    };
    tab.core_epoch = 8;
    readers
        .handle(
            action(
                &shared,
                DataSource::Frames {
                    tab,
                    subscription,
                    token,
                },
            ),
            &path,
            &client,
            &shared,
            &ctx,
        )
        .await;
    let mut peer = offer(&listener).await;
    bound(&mut core).await;
    frame(&mut peer, 1, 8).await;
    notice(&mut notices, DataNotice::Ready).await;
    readers.stop(&shared, &ctx).await;
    eof(&mut peer).await;
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
}

#[tokio::test]
async fn repeated_reset_joins_old_tags_before_reserving_more_readers_and_never_misdelivers_cancel()
{
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    let mut old_peer = None;
    for tag in 0..(MAX_FRAME_SUBSCRIPTIONS * 2) as u64 {
        let mut request = action(&shared, source());
        request.tag = tag;
        readers.handle(request, &path, &client, &shared, &ctx).await;
        // Assert before awaiting EOF so a regression fails rather than hanging.
        assert_eq!(readers.jobs.len(), 1);
        if let Some(mut peer) = old_peer.take() {
            eof(&mut peer).await;
        }
        let peer = offer(&listener).await;
        bound(&mut core).await;
        notice(&mut notices, DataNotice::Started).await;
        shared
            .lock()
            .unwrap()
            .features
            .data
            .reset(FeatureId::Browser);
        assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
        assert!(notices.try_recv().is_err());
        old_peer = Some(peer);
    }
    readers.stop(&shared, &ctx).await;
    eof(&mut old_peer.unwrap()).await;
    assert!(readers.jobs.is_empty());
    assert!(notices.try_recv().is_err());
}

#[tokio::test]
async fn typed_cancel_joins_and_releases_a_bind_but_a_pre_reset_cancel_cannot_cancel_its_replacement()
 {
    let (_dir, listener, path) = setup_listener();
    let (client, mut core) = crate::core_client::connection::tests::connect().await;
    let (shared, mut notices) = shared();
    let ctx = egui::Context::default();
    let mut readers = DataReaders::default();
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut peer = offer(&listener).await;
    let pending_bind = core.recv_request().await.unwrap();
    let mut cancel = action(&shared, source());
    cancel.request = DataRequest::Cancel;
    readers.handle(cancel, &path, &client, &shared, &ctx).await;
    assert_eq!(notices.try_recv().unwrap(), DataNotice::Cancelled);
    assert!(readers.jobs.is_empty());
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    eof(&mut peer).await;
    core.reply(
        &pending_bind.request_id,
        ReplyResult::Success(ReplyPayload::Command(CommandReply::accepted())),
    );

    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    let mut old_peer = offer(&listener).await;
    bound(&mut core).await;
    notice(&mut notices, DataNotice::Started).await;
    let mut stale_cancel = action(&shared, source());
    stale_cancel.request = DataRequest::Cancel;
    shared
        .lock()
        .unwrap()
        .features
        .data
        .reset(FeatureId::Browser);
    readers
        .handle(action(&shared, source()), &path, &client, &shared, &ctx)
        .await;
    eof(&mut old_peer).await;
    let mut replacement = offer(&listener).await;
    bound(&mut core).await;
    notice(&mut notices, DataNotice::Started).await;
    readers
        .handle(stale_cancel, &path, &client, &shared, &ctx)
        .await;
    assert_eq!(readers.jobs.len(), 1);
    assert!(notices.try_recv().is_err());
    frame(&mut replacement, 1, 7).await;
    notice(&mut notices, DataNotice::Ready).await;
    assert_eq!(shared.lock().unwrap().features.data.usage().1, 4);
    let mut cancel = action(&shared, source());
    cancel.request = DataRequest::Cancel;
    readers.handle(cancel, &path, &client, &shared, &ctx).await;
    assert_eq!(notices.try_recv().unwrap(), DataNotice::Cancelled);
    assert!(readers.jobs.is_empty());
    assert_eq!(shared.lock().unwrap().features.data.usage(), (0, 0, 0));
    eof(&mut replacement).await;
}
