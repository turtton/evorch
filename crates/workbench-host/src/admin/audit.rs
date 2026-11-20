//! Only this module finalizes management operations. Leaves may bind restart IDs.
use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use storage::admin_audit::*;
use tokio::sync::oneshot;
pub const ADMIN_IO_ACK_BUDGET: std::time::Duration = std::time::Duration::from_secs(10);
pub trait AuditPort: Send + Sync + 'static {
    fn submit(
        &self,
        write: AuditWrite,
        reply: AuditReplyCallback,
    ) -> Result<(), storage::StorageError>;
}
impl AuditPort for storage::StorageHandle {
    fn submit(
        &self,
        write: AuditWrite,
        reply: AuditReplyCallback,
    ) -> Result<(), storage::StorageError> {
        self.try_submit_admin_audit(write, reply)
    }
}
#[derive(Clone)]
pub struct AdminAudit {
    port: Arc<dyn AuditPort>,
    gate: crate::bootstrap::restart::HostRestartGate,
    pending: Arc<PendingAccepts>,
}
/// Includes in-flight and ACK-unknown admissions. Saturation refuses a new
/// write before enqueue; it never evicts an unresolved nonce or its receipt.
pub const MAX_UNCERTAIN_ACCEPTS: usize = 32;
struct PendingAccept {
    expected: AuditAccepted,
    receipt: Option<AuditRecord>,
    abandoned: bool,
    outcome: AuditOutcome,
}
#[derive(Default)]
struct PendingAccepts {
    rows: std::sync::Mutex<Vec<PendingAccept>>,
    changed: tokio::sync::Notify,
}
impl PendingAccepts {
    fn reserve(&self, expected: AuditAccepted, outcome: AuditOutcome) -> Result<(), AdminFailure> {
        let mut rows = self.rows.lock().unwrap();
        if rows.len() >= MAX_UNCERTAIN_ACCEPTS
            || rows
                .iter()
                .any(|r| r.expected.key.operation_id == expected.key.operation_id)
        {
            return Err(busy());
        }
        rows.push(PendingAccept {
            expected,
            receipt: None,
            abandoned: false,
            outcome,
        });
        Ok(())
    }
    fn remove(&self, key: &AuditOperationKey) {
        self.rows.lock().unwrap().retain(|r| r.expected.key != *key);
    }
    fn abandon(&self, key: &AuditOperationKey) {
        if let Some(row) = self
            .rows
            .lock()
            .unwrap()
            .iter_mut()
            .find(|r| r.expected.key == *key)
        {
            row.abandoned = true;
        }
        self.changed.notify_one();
    }
    fn observe(&self, reply: &Result<AuditReply, storage::StorageError>, late: bool) {
        let record = match reply {
            Ok(AuditReply::Reservation(
                AuditReservation::Created(r) | AuditReservation::Existing(r),
            ))
            | Ok(AuditReply::Applied(r))
            | Ok(AuditReply::Record(Some(r))) => r,
            _ => return,
        };
        let mut rows = self.rows.lock().unwrap();
        let Some(index) = rows.iter().position(|r| {
            r.expected.key == record.accepted.key && same_intent(&r.expected, &record.accepted)
        }) else {
            return;
        };
        if rows[index].abandoned && record.state == AuditState::Finished {
            rows.remove(index);
            return;
        }
        rows[index].receipt = Some(record.clone());
        if rows[index].abandoned && late {
            self.changed.notify_one();
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditReceipt {
    Durable,
    Unknown,
}
impl AdminAudit {
    pub fn new(port: Arc<dyn AuditPort>, gate: crate::bootstrap::restart::HostRestartGate) -> Self {
        Self {
            port,
            gate,
            pending: Arc::new(PendingAccepts::default()),
        }
    }
    pub(crate) fn effect_admission(
        &self,
    ) -> Result<crate::bootstrap::restart::HostCommandPermit, AdminFailure> {
        self.gate
            .enter(crate::bootstrap::restart::CommandAccess::ExistingInput)
            .map_err(|_| busy())
    }
    pub(crate) async fn write(&self, write: AuditWrite) -> Result<AuditReply, AdminFailure> {
        let permit = self.gate.enter_admin_audit().map_err(|_| busy())?;
        self.write_admitted(write, permit).await
    }
    async fn write_admitted(
        &self,
        write: AuditWrite,
        permit: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<AuditReply, AdminFailure> {
        let pending = self.pending.clone();
        let (tx, rx) = oneshot::channel();
        self.port
            .submit(
                write,
                Box::new(move |reply| {
                    // The response budget does not cancel a writer command. Keep
                    // sealing fenced until its callback (or command drop), even
                    // when the caller has already received ACK-unknown.
                    let _permit = permit;
                    pending.observe(&reply, false);
                    if let Err(reply) = tx.send(reply) {
                        pending.observe(&reply, true);
                    }
                }),
            )
            .map_err(|_| busy())?;
        tokio::time::timeout(ADMIN_IO_ACK_BUDGET, rx)
            .await
            .map_err(|_| unknown())?
            .map_err(|_| unknown())?
            .map_err(|_| unknown())
    }
    pub(crate) async fn background_write(
        &self,
        write: AuditWrite,
        producers: &runtime::producer::ProducerGate,
    ) -> Result<AuditReply, AdminFailure> {
        let _producer = producers.try_enter().ok_or_else(busy)?;
        let permit = self
            .gate
            .enter(crate::bootstrap::restart::CommandAccess::NewWork)
            .map_err(|_| busy())?;
        self.write_admitted(write, permit).await
    }
    pub(crate) async fn background_cancel(
        &self,
        entry: AuditAccepted,
        outcome: AdminOutcome,
        producers: &runtime::producer::ProducerGate,
    ) -> Result<(), AdminFailure> {
        let key = entry.key.clone();
        let outcome = match outcome {
            AdminOutcome::Succeeded => AuditOutcome::Succeeded,
            AdminOutcome::Cancelled => AuditOutcome::Cancelled,
            AdminOutcome::Conflict => AuditOutcome::Conflict,
            _ => AuditOutcome::Unknown,
        };
        self.pending.reserve(entry.clone(), outcome)?;
        let result = async {
            if let AuditReply::Reservation(AuditReservation::Created(_)) = self
                .background_write(AuditWrite::Accept(entry), producers)
                .await?
            {
                self.background_write(
                    AuditWrite::Finalize {
                        key: key.clone(),
                        outcome,
                        at_ms: now_ms(),
                    },
                    producers,
                )
                .await?;
            }
            Ok(())
        }
        .await;
        if result.is_ok() {
            self.pending.remove(&key);
        } else {
            self.pending.abandon(&key);
        }
        result
    }
    pub(crate) fn pending_entry(&self, operation_id: &str) -> Option<AuditAccepted> {
        self.pending
            .rows
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.expected.key.operation_id == operation_id)
            .map(|r| r.expected.clone())
    }
    pub(crate) async fn pending_changed(&self) {
        self.pending.changed.notified().await;
    }
    pub(crate) async fn reap_pending(&self, producers: &runtime::producer::ProducerGate) {
        let rows: Vec<_> = self
            .pending
            .rows
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.abandoned)
            .map(|r| (r.expected.clone(), r.outcome))
            .collect();
        for (entry, outcome) in rows {
            let key = entry.key.clone();
            let row = match self
                .background_write(
                    AuditWrite::Read {
                        operation_id: key.operation_id.clone(),
                    },
                    producers,
                )
                .await
            {
                Ok(AuditReply::Record(Some(r))) => r,
                Ok(AuditReply::Record(None)) => {
                    self.pending.remove(&key);
                    continue;
                }
                _ => continue,
            };
            // FIFO readback of a different nonce can never authorize touching it.
            if row.accepted.key != key || row.state == AuditState::Finished {
                self.pending.remove(&key);
                continue;
            }
            if !same_intent(&entry, &row.accepted) {
                continue;
            }
            let outcome = if row.state == AuditState::Prepared {
                outcome
            } else {
                AuditOutcome::Unknown
            };
            if matches!(
                self.background_write(
                    AuditWrite::Finalize {
                        key: key.clone(),
                        outcome,
                        at_ms: now_ms()
                    },
                    producers
                )
                .await,
                Ok(AuditReply::Applied(_))
            ) {
                self.pending.remove(&key);
            }
        }
    }
    pub(crate) async fn record(
        &self,
        key: &AuditOperationKey,
    ) -> Result<Option<AuditRecord>, AdminFailure> {
        match self
            .write(AuditWrite::Read {
                operation_id: key.operation_id.clone(),
            })
            .await?
        {
            AuditReply::Record(r) => Ok(r),
            _ => Err(unknown()),
        }
    }
    pub(crate) async fn accept(
        &self,
        entry: AuditAccepted,
    ) -> Result<AuditReservation, AdminFailure> {
        let key = entry.key.clone();
        self.pending
            .reserve(entry.clone(), AuditOutcome::AbortedBeforeEffect)?;
        let result = self.accept_inner(entry).await;
        if result.is_ok() {
            self.pending.remove(&key);
        } else {
            self.pending.abandon(&key);
        }
        result
    }
    async fn accept_inner(&self, entry: AuditAccepted) -> Result<AuditReservation, AdminFailure> {
        let key = entry.key.clone();
        let expected = entry.clone();
        match self.write(AuditWrite::Accept(entry)).await {
            Ok(AuditReply::Reservation(reservation)) => {
                let r = match &reservation {
                    AuditReservation::Created(r) | AuditReservation::Existing(r) => r,
                };
                if !same_intent(&expected, &r.accepted) {
                    return Err(conflict());
                }
                match reservation {
                    AuditReservation::Created(r)
                        if r.accepted.key == key && r.state == AuditState::Prepared =>
                    {
                        Ok(AuditReservation::Created(r))
                    }
                    AuditReservation::Created(r) | AuditReservation::Existing(r) => {
                        Ok(AuditReservation::Existing(r))
                    }
                }
            }
            _ => match self.record(&key).await? {
                Some(r) if !same_intent(&expected, &r.accepted) => Err(conflict()),
                Some(r) if r.accepted.key == key && r.state == AuditState::Prepared => {
                    Ok(AuditReservation::Created(r))
                }
                Some(r) => Ok(AuditReservation::Existing(r)),
                None => Err(unknown()),
            },
        }
    }
    pub(crate) async fn start(&self, key: &AuditOperationKey) -> Result<(), AdminFailure> {
        match self.write(AuditWrite::Start { key: key.clone() }).await {
            Ok(AuditReply::Applied(_)) => Ok(()),
            _ => match self.record(key).await? {
                Some(r) if r.accepted.key == *key && r.state == AuditState::Started => Ok(()),
                _ => Err(unknown()),
            },
        }
    }
    pub(crate) async fn finalize(
        &self,
        key: &AuditOperationKey,
        outcome: AuditOutcome,
    ) -> AuditReceipt {
        match self
            .write(AuditWrite::Finalize {
                key: key.clone(),
                outcome,
                at_ms: now_ms(),
            })
            .await
        {
            Ok(AuditReply::Applied(_)) => AuditReceipt::Durable,
            _ => match self.record(key).await {
                Ok(Some(r)) if r.accepted.key == *key && r.state == AuditState::Finished => {
                    AuditReceipt::Durable
                }
                _ => AuditReceipt::Unknown,
            },
        }
    }
    pub async fn reconcile(&self, mode: AuditReconcile) -> Result<(), AdminFailure> {
        for _ in 0..=10_000 / 64 {
            match self
                .write(AuditWrite::Reconcile {
                    mode: mode.clone(),
                    now_ms: now_ms(),
                })
                .await?
            {
                AuditReply::Cleanup(c) if c.more => {}
                AuditReply::Cleanup(_) => return Ok(()),
                _ => return Err(unknown()),
            }
        }
        Err(busy())
    }
    pub(crate) fn binding(self: &Arc<Self>, key: AuditOperationKey) -> RestartAuditBinding {
        RestartAuditBinding {
            audit: self.clone(),
            key,
            bound: Arc::new(AtomicBool::new(false)),
        }
    }
}
#[derive(Clone)]
pub struct RestartAuditBinding {
    audit: Arc<AdminAudit>,
    key: AuditOperationKey,
    bound: Arc<AtomicBool>,
}
impl RestartAuditBinding {
    pub async fn bind(
        &self,
        restart_id: &workbench_protocol::namespaces::lifecycle::AdminId,
    ) -> Result<(), AdminFailure> {
        match self
            .audit
            .write(AuditWrite::BindRestart {
                key: self.key.clone(),
                restart_id: restart_id.as_str().to_owned(),
            })
            .await
        {
            Ok(AuditReply::Applied(_)) => {}
            _ => {
                let r = self.audit.record(&self.key).await?.ok_or_else(unknown)?;
                if r.accepted.key != self.key
                    || r.restart_id.as_deref() != Some(restart_id.as_str())
                {
                    return Err(unknown());
                }
            }
        }
        self.bound.store(true, Ordering::Release);
        Ok(())
    }
    pub(crate) fn is_bound(&self) -> bool {
        self.bound.load(Ordering::Acquire)
    }
}
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

pub(crate) fn same_intent(a: &AuditAccepted, b: &AuditAccepted) -> bool {
    a.operation == b.operation
        && a.surface == b.surface
        && a.principal == b.principal
        && a.accepted_core_epoch == b.accepted_core_epoch
        && a.intent_digest == b.intent_digest
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    struct Port(AtomicUsize);
    impl AuditPort for Port {
        fn submit(
            &self,
            _: AuditWrite,
            reply: AuditReplyCallback,
        ) -> Result<(), storage::StorageError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            reply(Err(storage::StorageError::Io("injected".into())));
            Ok(())
        }
    }
    #[tokio::test]
    async fn background_write_cannot_cross_capture_even_with_an_old_producer_permit() {
        let gate = crate::bootstrap::restart::HostRestartGate::default();
        let port = Arc::new(Port(AtomicUsize::new(0)));
        let audit = AdminAudit::new(port.clone(), gate.clone());
        let producer = runtime::producer::ProducerGate::default();
        let _old_permit = producer.try_enter().unwrap();
        assert!(
            audit
                .background_write(AuditWrite::Stats, &producer)
                .await
                .is_err()
        );
        assert_eq!(port.0.load(Ordering::SeqCst), 1);
        gate.transition(1, crate::bootstrap::restart::HostRestartPhase::Capturing);
        assert!(
            audit
                .background_write(AuditWrite::Stats, &producer)
                .await
                .is_err()
        );
        assert_eq!(port.0.load(Ordering::SeqCst), 1);
    }
    fn entry(n: usize) -> AuditAccepted {
        AuditAccepted {
            key: AuditOperationKey {
                operation_id: format!("uncertain-{n}"),
                attempt_nonce: uuid::Uuid::new_v4().to_string(),
            },
            operation: AuditOperation::Release,
            surface: AuditSurface::Cli,
            principal: AuditPrincipal::anonymous(),
            accepted_core_epoch: 7,
            intent_digest: "a".repeat(64),
            accepted_at_ms: 1,
        }
    }
    struct HeldPort(tokio::sync::mpsc::UnboundedSender<(AuditWrite, AuditReplyCallback)>);
    impl AuditPort for HeldPort {
        fn submit(
            &self,
            write: AuditWrite,
            reply: AuditReplyCallback,
        ) -> Result<(), storage::StorageError> {
            self.0
                .send((write, reply))
                .map_err(|_| storage::StorageError::Io("closed".into()))
        }
    }
    #[tokio::test(start_paused = true)]
    async fn late_accept_after_both_response_budgets_is_owned_and_reaped_without_execution() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let audit = Arc::new(AdminAudit::new(
            Arc::new(HeldPort(tx)),
            crate::bootstrap::restart::HostRestartGate::default(),
        ));
        let entry = entry(1);
        let key = entry.key.clone();
        let job = tokio::spawn({
            let audit = audit.clone();
            let entry = entry.clone();
            async move { audit.accept(entry).await }
        });
        let (write, accept_reply) = rx.recv().await.unwrap();
        assert!(matches!(write, AuditWrite::Accept(_)));
        tokio::time::advance(ADMIN_IO_ACK_BUDGET).await;
        let (write, read_reply) = rx.recv().await.unwrap();
        assert!(matches!(write, AuditWrite::Read { .. }));
        tokio::time::advance(ADMIN_IO_ACK_BUDGET).await;
        assert!(job.await.unwrap().is_err());
        assert_eq!(audit.pending_entry(&key.operation_id), Some(entry.clone()));
        let row = AuditRecord {
            accepted: entry,
            state: AuditState::Prepared,
            restart_id: None,
            outcome: None,
            finished_at_ms: None,
        };
        accept_reply(Ok(AuditReply::Reservation(AuditReservation::Created(
            row.clone(),
        ))));
        read_reply(Ok(AuditReply::Record(Some(row.clone()))));
        assert_eq!(
            audit.pending.rows.lock().unwrap()[0].receipt,
            Some(row.clone())
        );
        let producers = runtime::producer::ProducerGate::default();
        let reap = tokio::spawn({
            let audit = audit.clone();
            async move { audit.reap_pending(&producers).await }
        });
        let (write, reply) = rx.recv().await.unwrap();
        assert!(matches!(write, AuditWrite::Read { .. }));
        reply(Ok(AuditReply::Record(Some(row.clone()))));
        let (write, reply) = rx.recv().await.unwrap();
        let AuditWrite::Finalize {
            key: final_key,
            outcome,
            ..
        } = write
        else {
            panic!("must finalize, never Start or Accept")
        };
        assert_eq!(final_key, key);
        assert_eq!(outcome, AuditOutcome::AbortedBeforeEffect);
        reply(Ok(AuditReply::Applied(AuditRecord {
            state: AuditState::Finished,
            outcome: Some(outcome),
            finished_at_ms: Some(2),
            ..row
        })));
        reap.await.unwrap();
        assert!(audit.pending.rows.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn uncertain_registry_never_evicts_a_nonce_when_full() {
        let pending = PendingAccepts::default();
        for n in 0..MAX_UNCERTAIN_ACCEPTS {
            pending
                .reserve(entry(n), AuditOutcome::AbortedBeforeEffect)
                .unwrap();
        }
        assert!(
            pending
                .reserve(
                    entry(MAX_UNCERTAIN_ACCEPTS),
                    AuditOutcome::AbortedBeforeEffect
                )
                .is_err()
        );
        assert_eq!(pending.rows.lock().unwrap().len(), MAX_UNCERTAIN_ACCEPTS);
        let key = pending.rows.lock().unwrap()[0].expected.key.clone();
        pending.remove(&key);
        pending
            .reserve(
                entry(MAX_UNCERTAIN_ACCEPTS),
                AuditOutcome::AbortedBeforeEffect,
            )
            .unwrap();
    }
    #[tokio::test]
    async fn a_reaper_cannot_finalize_another_attempt_nonce() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let audit = Arc::new(AdminAudit::new(
            Arc::new(HeldPort(tx)),
            crate::bootstrap::restart::HostRestartGate::default(),
        ));
        let e = entry(1);
        audit
            .pending
            .reserve(e.clone(), AuditOutcome::AbortedBeforeEffect)
            .unwrap();
        audit.pending.abandon(&e.key);
        let mut other = e;
        other.key.attempt_nonce = uuid::Uuid::new_v4().to_string();
        let producers = runtime::producer::ProducerGate::default();
        let reap = tokio::spawn({
            let audit = audit.clone();
            async move { audit.reap_pending(&producers).await }
        });
        let (write, reply) = rx.recv().await.unwrap();
        assert!(matches!(write, AuditWrite::Read { .. }));
        reply(Ok(AuditReply::Record(Some(AuditRecord {
            accepted: other,
            state: AuditState::Prepared,
            restart_id: None,
            outcome: None,
            finished_at_ms: None,
        }))));
        reap.await.unwrap();
        assert!(audit.pending.rows.lock().unwrap().is_empty());
        assert!(rx.try_recv().is_err());
    }
}
