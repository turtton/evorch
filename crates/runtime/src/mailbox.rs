//! AgentRun 間で送受信される [`AgentMessage`] の inbox 実装。
//!
//! [`RunMailbox`] はプロセス内の run タスク間メッセージ配送を担う。容量制限
//! 付きキューで fail-closed バックプレッシャーを提供し、各変更後に単調増加する
//! バージョン番号を通知する。観測済み完了通知の除去だけでは新入力を通知しない。

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

mod completion;
pub(crate) use completion::{CompletionNotice, QueuedMessage};

use event_bus::AgentMessage;
use tokio::sync::watch;

/// [`RunMailbox`] の最大メッセージ保持数。容量超過は [`super::RuntimeError::MailboxFull`] となる。
pub const MAILBOX_CAPACITY: usize = 64;

/// [`RunMailbox::try_push`] が受け入れを拒否した理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PushError {
    Full,
    Closed,
}

/// 単一の AgentRun が受け取るメッセージを保持する inbox。
#[derive(Debug)]
pub struct RunMailbox {
    queue: Mutex<VecDeque<QueuedMessage>>,
    closed: Mutex<bool>,
    version: watch::Sender<u64>,
}

impl RunMailbox {
    /// 空の inbox を生成する。バージョンは 0 から始まる。
    pub fn new() -> Self {
        let (version, _) = watch::channel(0);
        Self {
            queue: Mutex::new(VecDeque::with_capacity(MAILBOX_CAPACITY)),
            closed: Mutex::new(false),
            version,
        }
    }

    /// メッセージを inbox の末尾に追加する。
    ///
    /// inbox が閉じている場合は [`PushError::Closed`]、容量一杯の場合は
    /// [`PushError::Full`] を返す。成功時にバージョンを単調増加させる。
    pub fn try_push(&self, message: AgentMessage) -> Result<(), PushError> {
        self.try_push_queued(QueuedMessage::explicit(message))
    }

    /// 自動完了通知を追加する。複数 run の `wait(mode=all)` はこの通知だけでは
    /// 中断せず、対象 run の終端状態で完了条件を判定する。
    pub(crate) fn try_push_completion(
        &self,
        message: AgentMessage,
        notice: std::sync::Arc<CompletionNotice>,
    ) -> Result<(), PushError> {
        self.try_push_queued(QueuedMessage::completion(message, notice))
    }

    pub(crate) fn try_push_queued(&self, queued: QueuedMessage) -> Result<(), PushError> {
        if queued.observed() {
            return Ok(());
        }
        let closed = self
            .closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if *closed {
            return Err(PushError::Closed);
        }
        let mut queue = self.pending();
        if queue.len() >= MAILBOX_CAPACITY {
            return Err(PushError::Full);
        }
        queue.push_back(queued);
        drop(queue);
        drop(closed);
        self.version.send_modify(|value| *value += 1);
        Ok(())
    }

    /// Filter before any queue consumer, including aside/Waiting drains and inbox.
    /// Pruning is not new input and deliberately does not change the version.
    fn pending(&self) -> MutexGuard<'_, VecDeque<QueuedMessage>> {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        queue.retain(|message| !message.observed());
        queue
    }

    pub(crate) fn discard_observed_completions(&self) {
        drop(self.pending());
    }

    /// 新規メッセージの受け入れを停止する。閉じた後の `try_push` は
    /// [`PushError::Closed`] を返す。既存メッセージは取得可能なまま残る。
    ///
    /// 終端待ちを version 購読者に通知するため、closed フラグ設定後にバージョンを
    /// 単調増加させる。
    pub fn close(&self) {
        let mut closed = self
            .closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *closed = true;
        drop(closed);
        self.version.send_modify(|value| *value += 1);
    }

    /// inbox が新規メッセージを受け付けていないかどうかを返す。
    pub fn is_closed(&self) -> bool {
        *self
            .closed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// すべてのメッセージを FIFO 順で取り出す。
    ///
    /// 取り出し後にバージョンを更新する。
    pub fn drain_all(&self) -> Vec<AgentMessage> {
        let mut queue = self.pending();
        let drained: Vec<AgentMessage> = queue.drain(..).map(|queued| queued.message).collect();
        drop(queue);
        self.version.send_modify(|value| *value += 1);
        drained
    }

    /// 条件に一致するすべてのメッセージを FIFO 順で取り出し、一致しないメッセージは
    /// 相対順序を保って残す。
    pub fn drain_where(&self, predicate: impl Fn(&AgentMessage) -> bool) -> Vec<AgentMessage> {
        let mut queue = self.pending();
        let mut matched = Vec::new();
        let mut remaining = VecDeque::with_capacity(queue.len());
        while let Some(queued) = queue.pop_front() {
            if predicate(&queued.message) {
                matched.push(queued.message);
            } else {
                remaining.push_back(queued);
            }
        }
        *queue = remaining;
        drop(queue);
        self.version.send_modify(|value| *value += 1);
        matched
    }

    /// 条件に一致する最初のメッセージを 1 件取り出す。一致するものがなければ
    /// `None` を返す。
    pub fn remove_first_where(
        &self,
        predicate: impl Fn(&AgentMessage) -> bool,
    ) -> Option<AgentMessage> {
        let mut queue = self.pending();
        let index = queue.iter().position(|queued| predicate(&queued.message));
        let removed = index
            .and_then(|index| queue.remove(index))
            .map(|queued| queued.message);
        drop(queue);
        if removed.is_some() {
            self.version.send_modify(|value| *value += 1);
        }
        removed
    }

    /// 現在のメッセージ数を返す。
    pub fn len(&self) -> usize {
        self.pending().len()
    }

    /// inbox が空かどうかを返す。
    pub fn is_empty(&self) -> bool {
        self.pending().is_empty()
    }

    /// 自動完了通知以外の未読メッセージがあり、run の待機を中断すべきか返す。
    /// メッセージの内容や既読状態は変更しない。
    pub(crate) fn has_interrupting_message(&self) -> bool {
        self.pending().iter().any(|queued| queued.interrupts_wait())
    }

    /// バージョン変更通知を受け取る購読者を返す。
    pub fn subscribe_version(&self) -> watch::Receiver<u64> {
        self.version.subscribe()
    }

    /// `sender` からの、自動完了通知以外の未読メッセージがあるか返す。
    pub(crate) fn has_interrupting_message_from(&self, sender: &str) -> bool {
        self.pending()
            .iter()
            .any(|queued| queued.interrupts_wait() && queued.message.sender_run_id == sender)
    }

    /// 自動完了通知以外の未読メッセージを FIFO 順で取り出し、完了通知は残す。
    pub(crate) fn drain_interrupting(&self) -> Vec<AgentMessage> {
        let mut queue = self.pending();
        let (taken, kept): (VecDeque<_>, VecDeque<_>) =
            queue.drain(..).partition(|queued| queued.interrupts_wait());
        *queue = kept;
        drop(queue);
        if !taken.is_empty() {
            self.version.send_modify(|value| *value += 1);
        }
        taken.into_iter().map(|queued| queued.message).collect()
    }
}

impl Default for RunMailbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use event_bus::{AgentMessage, AgentMessageKind};

    use super::*;

    fn sample(id: &str) -> AgentMessage {
        AgentMessage {
            message_id: id.to_string(),
            sender_run_id: "run-1".to_string(),
            recipient_run_id: "run-2".to_string(),
            kind: AgentMessageKind::Send,
            content: id.to_string(),
            reply_to: None,
        }
    }

    macro_rules! message_id {
        ($index:literal) => {
            concat!("msg-", stringify!($index))
        };
    }

    // Given: 3 件のメッセージを追加した inbox
    // When: drain_all する
    // Then: FIFO 順に取り出せる
    #[tokio::test]
    async fn drain_all_returns_messages_in_fifo_order() {
        let mailbox = RunMailbox::new();
        mailbox.try_push(sample(message_id!(1))).unwrap();
        mailbox.try_push(sample(message_id!(2))).unwrap();
        mailbox.try_push(sample(message_id!(3))).unwrap();

        let drained = mailbox.drain_all();

        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].message_id, "msg-1");
        assert_eq!(drained[1].message_id, "msg-2");
        assert_eq!(drained[2].message_id, "msg-3");
        assert!(mailbox.is_empty());
    }

    // Given: 既に drain_all した空 inbox
    // When: もう一度 drain_all する
    // Then: 空ベクトルが返る
    #[tokio::test]
    async fn drain_all_on_empty_mailbox_returns_empty() {
        let mailbox = RunMailbox::new();

        let drained = mailbox.drain_all();

        assert!(drained.is_empty());
    }

    // Given: 上限までメッセージを追加した inbox
    // When: さらに 1 件追加する
    // Then: MailboxFullError が返る
    #[tokio::test]
    async fn try_push_rejects_when_mailbox_full() {
        let mailbox = RunMailbox::new();
        for index in 1..=MAILBOX_CAPACITY {
            assert!(
                mailbox.try_push(sample(&format!("msg-{index}"))).is_ok(),
                "{index} 件目は追加できる"
            );
        }

        let result = mailbox.try_push(sample("overflow"));

        assert_eq!(result, Err(PushError::Full));
    }

    // Given: 複数の購読者がいる inbox
    // When: メッセージを追加する
    // Then: 各購読者がそれ以降のバージョン変化を検知する
    #[tokio::test]
    async fn subscribe_version_notifies_on_push() {
        let mailbox = RunMailbox::new();
        let rx1 = mailbox.subscribe_version();
        let rx2 = mailbox.subscribe_version();

        mailbox.try_push(sample("msg-1")).unwrap();

        assert_eq!(*rx1.borrow(), 1);
        assert_eq!(*rx2.borrow(), 1);
    }

    // Given: 条件一致しないメッセージを含む inbox
    // When: drain_where で一部のメッセージを取り出す
    // Then: 一致したものだけが FIFO 順で、残りは相対順序を保つ
    #[tokio::test]
    async fn drain_where_keeps_unmatched_messages_in_order() {
        let mailbox = RunMailbox::new();
        for id in ["msg-1", "msg-2", "msg-3", "msg-4"] {
            mailbox.try_push(sample(id)).unwrap();
        }

        let matched = mailbox
            .drain_where(|message| message.message_id == "msg-1" || message.message_id == "msg-4");

        assert_eq!(matched.len(), 2);
        assert_eq!(matched[0].message_id, "msg-1");
        assert_eq!(matched[1].message_id, "msg-4");
        let remaining = mailbox.drain_all();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].message_id, "msg-2");
        assert_eq!(remaining[1].message_id, "msg-3");
    }

    // Given: 同種条件を満たす複数のメッセージを含む inbox
    // When: remove_first_where する
    // Then: 最初に一致する 1 件だけが取り除かれる
    #[tokio::test]
    async fn remove_first_where_removes_only_first_match() {
        let mailbox = RunMailbox::new();
        for id in ["msg-1", "msg-2", "msg-3"] {
            mailbox.try_push(sample(id)).unwrap();
        }

        let removed = mailbox.remove_first_where(|message| message.message_id == "msg-2");

        assert_eq!(
            removed.map(|message| message.message_id),
            Some("msg-2".to_string())
        );
        let remaining = mailbox.drain_all();
        assert_eq!(remaining.len(), 2);
        assert_eq!(remaining[0].message_id, "msg-1");
        assert_eq!(remaining[1].message_id, "msg-3");
    }
}
