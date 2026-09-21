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
    fences: std::sync::Arc<crate::fencing::MutationFences>,
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
            fences: std::sync::Arc::default(),
        }
    }

    /// イベントを全受信者へブロードキャストし、受信者数を返す。
    ///
    /// 受信者がゼロの場合は [`broadcast::SendError`] となるが、これを受信者数
    /// 0 として扱い panic しない。
    pub fn emit(&self, event: Event) -> usize {
        let Some(_guards) = self.fences.acquire(&event) else {
            return 0;
        };
        self.tx.send(event).unwrap_or(0)
    }

    pub fn register_mutation_fence(&self, run: String, check: crate::MutationCheck) -> bool {
        self.fences.register(run, check)
    }

    pub fn register_mutation_guard(&self, run: String, check: crate::MutationGuardCheck) -> bool {
        self.fences.register_guard(run, check)
    }

    pub fn mutation_validator(&self) -> crate::MutationValidator {
        crate::MutationValidator::new(std::sync::Arc::clone(&self.fences))
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
            lag_gate: LagGate::default(),
            fences: std::sync::Arc::clone(&self.fences),
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
    fences: std::sync::Arc<crate::fencing::MutationFences>,
    rx: broadcast::Receiver<Event>,
    /// fault 再 emit 用の送信者クローン。
    tx: broadcast::Sender<Event>,
    subscriber_id: u64,
    /// lag エピソードごとの warn と fault の発行を制御するゲート。
    lag_gate: LagGate,
}

#[derive(Default)]
struct LagGate {
    episode_active: bool,
}

impl LagGate {
    fn note_lag(&mut self) -> (bool, bool) {
        if self.episode_active {
            (false, false)
        } else {
            self.episode_active = true;
            (true, true)
        }
    }

    fn note_ok(&mut self) {
        self.episode_active = false;
    }
}

impl EventReceiver {
    pub fn mutation_validator(&self) -> crate::MutationValidator {
        crate::MutationValidator::new(std::sync::Arc::clone(&self.fences))
    }
    /// この受信者に割り当てられた `subscriber_id` を返す。
    pub fn subscriber_id(&self) -> u64 {
        self.subscriber_id
    }

    /// 次のイベントを受信する。
    ///
    /// # 戻り値
    ///
    /// - `Ok(event)`: 受信成功。lag エピソードをリセットする。
    /// - `Err(RecvError::Lagged(n))`: 受信者が `n` 件のイベントを読み飛ばした。
    ///   lag エピソードの最初だけ `tracing::warn!` を発火し、
    ///   [`FaultEvent::SubscriberLagged`] をバスへ emit する。同じエピソード中の
    ///   後続 lag は `tracing::debug!` のみ発火する。
    /// - `Err(RecvError::Closed)`: 全ての送信者（[`EventReceiver`] 内部の
    ///   クローンを含む）が drop された後。
    ///
    /// # Lag ポリシーと fault 抑制フラグについて
    ///
    /// 受信者がチャネル容量を超えて取り残されると、tokio は `Lagged(n)` を返し、
    /// 受信者を「最も古い保持中メッセージ」へ再配置する。本メソッドは 1 つの lag
    /// エピソード（次に raw `Ok` を受信するまで）につき 1 回だけ
    /// `tracing::warn!` と [`FaultEvent::SubscriberLagged`] の emit を行う。同じ
    /// エピソード中の後続 `Lagged` は `tracing::debug!` のみ発火する。
    ///
    /// fault の再 emit を無条件に行うとフィードバックループが発生する。`Lagged`
    /// 後に受信者は最も古い保持中メッセージへ再配置されるが、その直後に fault を
    /// emit するとバスの head が進み、容量一杯のチャネルからちょうどその
    /// メッセージが押し出される。結果として受信者は再び `Lagged(1)` を受け取り、
    /// また fault を emit するという無限ループに陥る。`LagGate` は、`Ok` で
    /// 正常に受信できるまで fault の再 emit を止めることで、この
    /// fault 再 emit が自らの lag を誘発するループを防ぐために存在する。
    pub async fn recv(&mut self) -> Result<Event, RecvError> {
        loop {
            match self.rx.recv().await {
                Ok(event) => {
                    self.lag_gate.note_ok();
                    if self.fences.accepts(&event) {
                        return Ok(event);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(skipped)) => {
                    let (warn, fault) = self.lag_gate.note_lag();
                    if warn {
                        tracing::warn!(
                            subscriber_id = self.subscriber_id,
                            skipped = skipped,
                            "event subscriber lagged; dropped events"
                        );
                    } else {
                        tracing::debug!(
                            subscriber_id = self.subscriber_id,
                            skipped = skipped,
                            "event subscriber lagged; dropped events"
                        );
                    }
                    if fault {
                        // 受信者がゼロの場合 fault は届かないが、それは観測者が
                        // 存在しないことと同義であるため送信結果は無視してよい。
                        let _ = self.tx.send(Event::new(FaultEvent::SubscriberLagged {
                            subscriber_id: self.subscriber_id,
                            skipped,
                        }));
                    }
                    return Err(RecvError::Lagged(skipped));
                }
                Err(broadcast::error::RecvError::Closed) => return Err(RecvError::Closed),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LagGate;

    #[test]
    fn lag_gate_warns_and_faults_once_per_episode() {
        let mut gate = LagGate::default();

        assert_eq!(gate.note_lag(), (true, true));
        assert_eq!(gate.note_lag(), (false, false));

        gate.note_ok();

        assert_eq!(gate.note_lag(), (true, true));
    }

    #[test]
    fn lag_gate_resets_on_raw_ok_before_fencing() {
        let mut gate = LagGate::default();

        assert_eq!(gate.note_lag(), (true, true));
        gate.note_ok();

        assert_eq!(gate.note_lag(), (true, true));
    }
}
