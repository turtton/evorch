//! [`EventBus`] と [`EventReceiver`] の統合テスト。
//!
//! lag はチャネル容量（2 の冪）と emit 数による決定論的な容量計算のみで作り
//! 出し、時間ベースの sleep は使用しない。唯一の例外は、抑制中に 2 つ目の
//! fault が emit されないことを確認する短い `timeout` による否定アサートである。

use std::time::Duration;

use event_bus::bus::{EventBus, RecvError};
use event_bus::event::{Event, EventKind, FaultEvent, LifecycleEvent, MessageEvent, ToolEvent};

/// インデックスごとに異なる種別のイベントを生成するヘルパー。
fn sample_event(index: usize) -> Event {
    let kind: EventKind = match index % 3 {
        0 => LifecycleEvent::Started {
            session_id: format!("session-{index}"),
        }
        .into(),
        1 => MessageEvent::MessageDelta {
            delta: format!("delta-{index}"),
        }
        .into(),
        _ => ToolEvent::ToolStarted {
            tool_name: "read".to_string(),
            call_id: format!("call-{index}"),
            run_id: None,
        }
        .into(),
    };
    Event::new(kind)
}

#[tokio::test]
async fn multi_subscriber_receives_all_events_in_order() {
    let bus = EventBus::new(16);
    let mut rx1 = bus.subscribe();
    let mut rx2 = bus.subscribe();
    assert_eq!(bus.receiver_count(), 2);

    let sent: Vec<Event> = (0..3).map(sample_event).collect();
    for event in &sent {
        bus.emit(event.clone());
    }

    for expected in &sent {
        assert_eq!(&rx1.recv().await.unwrap(), expected);
        assert_eq!(&rx2.recv().await.unwrap(), expected);
    }
}

#[tokio::test]
async fn emit_with_zero_subscribers_returns_zero_and_does_not_panic() {
    let bus = EventBus::new(16);

    assert_eq!(bus.emit(sample_event(0)), 0);
    assert_eq!(bus.receiver_count(), 0);
}

#[tokio::test]
async fn lagged_receiver_emits_single_fault_per_episode() {
    // 容量 2 のチャネルに e1..e4 を emit すると e3, e4 のみが残存し、
    // poll していない rx は e1, e2 を取りこぼす。
    let bus = EventBus::new(2);
    let mut rx = bus.subscribe();

    let e1 = sample_event(0);
    let e2 = sample_event(1);
    let e3 = sample_event(2);
    let e4 = sample_event(3);
    bus.emit(e1.clone());
    bus.emit(e2.clone());
    bus.emit(e3.clone());
    bus.emit(e4.clone());

    // observer は現在の末尾（e4 の後）から受信を開始する。
    let mut observer = bus.subscribe();

    // rx は e1, e2 を取りこぼした。
    assert_eq!(rx.recv().await, Err(RecvError::Lagged(2)));

    // Lagged により fault が emit され、observer がそれを受信する。
    let fault = observer.recv().await.unwrap();
    assert_eq!(
        fault.kind,
        EventKind::Fault(FaultEvent::SubscriberLagged {
            subscriber_id: rx.subscriber_id(),
            skipped: 2,
        })
    );

    // fault emit で head が進み、rx の再配置先（e3）が押し出されて Lagged(1)。
    // suppression フラグにより 2 つ目の fault は emit されない。
    assert_eq!(rx.recv().await, Err(RecvError::Lagged(1)));

    // 抑制中は fault が増えないことを短い timeout で確認する（否定アサート）。
    let quiet = tokio::time::timeout(Duration::from_millis(50), observer.recv()).await;
    assert!(
        quiet.is_err(),
        "suppressed lag must not emit a second fault"
    );

    // rx は保持されていた e4 を受信し、Ok 受信でフラグがリセットされる。
    assert_eq!(rx.recv().await.unwrap(), e4);

    // チャネルには fault が保持されており、rx がそれを受信する。
    let echoed = rx.recv().await.unwrap();
    assert_eq!(
        echoed.kind,
        EventKind::Fault(FaultEvent::SubscriberLagged {
            subscriber_id: rx.subscriber_id(),
            skipped: 2,
        })
    );

    // 第 2 の lag エピソード: observer を drop し、rx が poll しないまま
    // e5..e8 を emit する。容量 2 のリングは e5, e6 を押し出して [e7, e8]
    // を保持するため、rx（次期待値は e5）は 2 件を取りこぼす。
    drop(observer);
    bus.emit(sample_event(4));
    bus.emit(sample_event(5));
    bus.emit(sample_event(6));
    bus.emit(sample_event(7));

    // fault2 の emit を観測するため、rx.recv() より先に末尾で subscribe する。
    // （rx.recv() 内部で fault が emit された後に subscribe すると、
    //   broadcast が新規受信者を現在の末尾に配置するため fresh.recv() が
    //   永遠にブロックする）
    let mut fresh = bus.subscribe();

    // フラグは Ok 受信でリセット済みのため、再び fault が emit される。
    assert_eq!(rx.recv().await, Err(RecvError::Lagged(2)));

    let second_fault = fresh.recv().await.unwrap();
    assert_eq!(
        second_fault.kind,
        EventKind::Fault(FaultEvent::SubscriberLagged {
            subscriber_id: rx.subscriber_id(),
            skipped: 2,
        })
    );
}

#[tokio::test]
async fn receiver_count_reflects_dropped_bus() {
    let bus = EventBus::new(16);
    let rx = bus.subscribe();
    assert_eq!(bus.receiver_count(), 1);

    // 受信者を drop すると receiver_count() は 0 を返す。
    drop(rx);
    assert_eq!(bus.receiver_count(), 0);

    // bus を drop しても EventReceiver が Sender のクローンを保持する限り
    // チャネルは閉じない。RecvError::Closed は「送信者（EventReceiver 内部の
    // クローンを含む）が全て drop された」場合のための予約 Variant であり、
    // 受信者を保持し続ける通常の構成では観測できない。
    drop(bus);
}
