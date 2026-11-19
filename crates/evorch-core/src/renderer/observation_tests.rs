//! Fake source through the actual core data pump, without a renderer.
use super::*;
use workbench_host::areas::browser::observation::{
    ObservedFrame, ObservedPage, fake::FakeObservationPort, service::ObservationService,
};
use workbench_host::connections::ConnectionHooks;
use workbench_protocol::{
    envelope::{Provenance, Surface},
    ids::{ClientId, ProjectId, RunId, ThreadId},
    namespaces::browser::{
        BrowserCommand, BrowserQuery,
        observation::{data::AgentFrameStart, *},
    },
};
#[tokio::test]
async fn fake_subscribe_bind_wire_ack_cancel_joins_without_renderer() {
    let scope = ObservationScope {
        project: ProjectId::new("p").unwrap(),
        thread: ThreadId::new("t").unwrap(),
        run: RunId::new("r").unwrap(),
        incarnation: RunId::new("i").unwrap(),
        core_epoch: 7,
        browser_generation: 1,
        page_generation: 1,
    };
    let tab = ObservationId::new("a".repeat(32)).unwrap();
    let start = AgentFrameStart {
        scope: scope.clone(),
        tab: tab.clone(),
        sequence: 1,
        width: 1,
        height: 1,
        stride: 4,
        total_bytes: 4,
    };
    let port = Arc::new(FakeObservationPort::default());
    let mut page = ObservedPage {
        snapshot: ObservationSnapshot {
            scope: scope.clone(),
            revision: 1,
            state: ObservationState::Live,
            tabs: vec![ObservationTab {
                id: tab.clone(),
                title: "page".into(),
                url: "https://example.test".into(),
            }],
            active_tab: Some(tab),
            activity: vec![],
        },
        frame: Some(Arc::new(ObservedFrame {
            start: start.clone(),
            rgba: Arc::from([1, 2, 3, 255]),
        })),
    };
    assert!(port.publish(page.clone()));
    let plane = DataPlane::default();
    let hooks = Arc::new(ConnectionHooks::default());
    let client = ClientId::new("desktop").unwrap();
    let _connection = hooks.open(client.clone());
    let p = Provenance::human(Surface::Desktop, client);
    let service = ObservationService::new(Some(port.clone()), plane.clone(), hooks, 7);
    let ObservationResult::Subscribed { subscription, .. } = service
        .command(BrowserCommand::SubscribeAgentFrames { scope }, &p)
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
            &p,
        )
        .unwrap()
    else {
        panic!()
    };
    let channel = plane.open_channel().unwrap();
    service
        .command(
            BrowserCommand::BindAgentData {
                subscription: subscription.clone(),
                token,
                channel_id: channel.id().clone(),
            },
            &p,
        )
        .unwrap();
    let Lease::AgentFrames(lease) = channel.bound().await.unwrap() else {
        panic!()
    };
    let (ours, mut peer) = tokio::net::UnixStream::pair().unwrap();
    let (read, write) = ours.into_split();
    let (_stop, mut stopped) = watch::channel(false);
    let sender = agent_frames(lease, read, write, &mut stopped);
    let receiver = async {
        let mut bytes = vec![0; start.encode().unwrap().len()];
        peer.read_exact(&mut bytes).await.unwrap();
        assert_eq!(
            Prefix::parse(bytes[..PREFIX_LEN].try_into().unwrap())
                .unwrap()
                .kind,
            Kind::AgentFrameStart
        );
        assert_eq!(
            AgentFrameStart::decode(&bytes[PREFIX_LEN..]).unwrap(),
            start
        );
        let mut chunk = vec![0; PREFIX_LEN + ChunkMeta::LEN + 4];
        peer.read_exact(&mut chunk).await.unwrap();
        assert_eq!(&chunk[PREFIX_LEN + ChunkMeta::LEN..], &[1, 2, 3, 255]);
        let mut end = vec![0; PREFIX_LEN + EndMeta::LEN];
        peer.read_exact(&mut end).await.unwrap();
        assert_eq!(EndMeta::decode(&end[PREFIX_LEN..]).unwrap().total_bytes, 4);
        peer.write_all(&AckMeta { id: 1 }.encode()).await.unwrap();
        // Receiving a second frame proves the first ACK was consumed. Cancel
        // with no unread ACK queued, while the next transfer awaits its ACK.
        page.snapshot.revision = 2;
        Arc::make_mut(page.frame.as_mut().unwrap()).start.sequence = 2;
        assert!(port.publish(page));
        peer.read_exact(&mut bytes).await.unwrap();
        assert_eq!(
            AgentFrameStart::decode(&bytes[PREFIX_LEN..])
                .unwrap()
                .sequence,
            2
        );
        peer.read_exact(&mut chunk).await.unwrap();
        peer.read_exact(&mut end).await.unwrap();
        service
            .command(BrowserCommand::UnsubscribeAgentFrames { subscription }, &p)
            .unwrap();
        let mut closed = Vec::new();
        peer.read_to_end(&mut closed).await.unwrap();
        assert_eq!(
            Prefix::parse(closed[..PREFIX_LEN].try_into().unwrap())
                .unwrap()
                .kind,
            Kind::Error
        );
        assert_eq!(
            ErrorMeta::decode(&closed[PREFIX_LEN..]).unwrap().code,
            ErrorCode::Closed
        );
    };
    let (sent, ()) = tokio::join!(sender, receiver);
    sent.unwrap();
}
