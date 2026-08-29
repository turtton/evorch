//! tokio broadcast チャネル上に構築されたイベントバス。
//!
//! [`EventBus`] は単一の [`broadcast::Sender`] を所有し、[`Event`] を全受信者へ
//! ブロードキャストする。各受信者（[`EventReceiver`]）には単調増加する
//! `subscriber_id` が割り当てられ、遅延（lag）を検出すると warn ログと fault
//! イベントの発行を行う。lag ポリシーの詳細は [`EventReceiver::recv`] の
//! ドキュメントを参照。

use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::broadcast;

use crate::event::{Event, FaultEvent};

/// ブロードキャスト型イベントバス。
///
/// `Arc<EventBus>` として共有することを想定しており、`Clone` 実装は意図的に
/// 提供しない。
pub struct EventBus {
    tx: broadcast::Sender<Event>,
    next_subscriber_id: AtomicU64,
}

impl EventBus {
    /// 指定した容量でイベントバスを生成する。
    ///
    /// # 注意
    ///
    /// `capacity` は tokio が 2 の冪に切り上げることに注意（例: `5` は `8` と
    /// して確保される）。`0` を指定した場合、内部で利用する
    /// [`broadcast::channel`] が panic する。
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity);
        Self {
            tx,
            next_subscriber_id: AtomicU64::new(0),
        }
    }

    /// イベントを全受信者へブロードキャストし、受信者数を返す。
    ///
    /// 受信者がゼロの場合は [`broadcast::SendError`] となるが、これを受信者数
    /// 0 として扱い panic しない。
    pub fn emit(&self, event: Event) -> usize {
        self.tx.send(event).unwrap_or(0)
    }

    /// 新しい受信者を登録し、単調増加する `subscriber_id` を割り当てる。
    ///
    /// 受信者は登録時点でチャネルに存在するイベントの末尾から受信を開始する
    /// （登録済みの過去イベントは受信しない）。
    pub fn subscribe(&self) -> EventReceiver {
        let subscriber_id = self.next_subscriber_id.fetch_add(1, Ordering::Relaxed);
        EventReceiver {
            rx: self.tx.subscribe(),
            tx: self.tx.clone(),
            subscriber_id,
            fault_suppressed: false,
        }
    }

    /// 現在の受信者数を返す。
    pub fn receiver_count(&self) -> usize {
        self.tx.receiver_count()
    }
}

/// [`EventReceiver::recv`] のエラー。
#[derive(Debug, PartialEq, Eq)]
pub enum RecvError {
    /// 全ての送信者（[`EventReceiver`] が内部に保持する `Sender` のクローンを
    /// 含む）が drop された。[`EventReceiver`] 自身が送信者を保持し続けるため、
    /// 受信者を生かしたままの通常の構成では到達しない予約 Variant である。
    Closed,
    /// 受信者が遅延し、`n` 件のイベントを読み飛ばした。
    Lagged(u64),
}

/// [`EventBus`] からイベントを受信するハンドル。
///
/// 内部に `Sender` のクローンを保持するため、[`EventBus`] が drop されても
/// この受信者が生きている限りチャネルは閉じない。
pub struct EventReceiver {
    rx: broadcast::Receiver<Event>,
    /// fault 再 emit 用の送信者クローン。
    tx: broadcast::Sender<Event>,
    subscriber_id: u64,
    /// フィードバックループ防止フラグ。直近の lag エピソードで fault を
    /// emit 済みの場合に true となる。役割の詳細は [`EventReceiver::recv`]。
    fault_suppressed: bool,
}

impl EventReceiver {
    /// この受信者に割り当てられた `subscriber_id` を返す。
    pub fn subscriber_id(&self) -> u64 {
        self.subscriber_id
    }

    /// 次のイベントを受信する。
    ///
    /// # 戻り値
    ///
    /// - `Ok(event)`: 受信成功。lag 抑制フラグをリセットする。
    /// - `Err(RecvError::Lagged(n))`: 受信者が `n` 件のイベントを読み飛ばした。
    ///   常に `tracing::warn!` を発火する。さらに、直近の `Ok` 受信以降に
    ///   fault を emit していない場合（`fault_suppressed == false`）は
    ///   [`FaultEvent::SubscriberLagged`] をバスへ emit してフラグを立てる。
    ///   フラグが立っている場合は warn のみで fault の再 emit は行わない。
    /// - `Err(RecvError::Closed)`: 全ての送信者（[`EventReceiver`] 内部の
    ///   クローンを含む）が drop された後。
    ///
    /// # Lag ポリシーと fault 抑制フラグについて
    ///
    /// 受信者がチャネル容量を超えて取り残されると、tokio は `Lagged(n)` を返し、
    /// 受信者を「最も古い保持中メッセージ」へ再配置する。本メソッドはその都度
    /// `tracing::warn!` を発火し、加えて 1 つの lag エピソード（次に `Ok` で
    /// 受信できるまで）につき 1 回だけ [`FaultEvent::SubscriberLagged`] をバスへ
    /// emit する。
    ///
    /// fault の再 emit を無条件に行うとフィードバックループが発生する。`Lagged`
    /// 後に受信者は最も古い保持中メッセージへ再配置されるが、その直後に fault を
    /// emit するとバスの head が進み、容量一杯のチャネルからちょうどその
    /// メッセージが押し出される。結果として受信者は再び `Lagged(1)` を受け取り、
    /// また fault を emit するという無限ループに陥る。`fault_suppressed` フラグ
    /// は、`Ok` で正常に受信できるまで fault の再 emit を止めることで、この
    /// fault 再 emit が自らの lag を誘発するループを防ぐために存在する。
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        match self.rx.recv().await {
            Ok(event) => {
                self.fault_suppressed = false;
                Ok(event)
            }
            Err(broadcast::error::RecvError::Lagged(skipped)) => {
                tracing::warn!(
                    subscriber_id = self.subscriber_id,
                    skipped = skipped,
                    "event subscriber lagged; dropped events"
                );
                if !self.fault_suppressed {
                    // 受信者がゼロの場合 fault は届かないが、それは観測者が
                    // 存在しないことと同義であるため送信結果は無視してよい。
                    let _ = self.tx.send(Event::new(FaultEvent::SubscriberLagged {
                        subscriber_id: self.subscriber_id,
                        skipped,
                    }));
                    self.fault_suppressed = true;
                }
                Err(RecvError::Lagged(skipped))
            }
            Err(broadcast::error::RecvError::Closed) => Err(RecvError::Closed),
        }
    }
}
