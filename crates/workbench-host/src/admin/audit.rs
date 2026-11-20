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
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuditReceipt {
    Durable,
    Unknown,
}
impl AdminAudit {
    pub fn new(port: Arc<dyn AuditPort>, gate: crate::bootstrap::restart::HostRestartGate) -> Self {
        Self { port, gate }
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
        let (tx, rx) = oneshot::channel();
        self.port
            .submit(
                write,
                Box::new(move |reply| {
                    // The response budget does not cancel a writer command. Keep
                    // sealing fenced until its callback (or command drop), even
                    // when the caller has already received ACK-unknown.
                    let _permit = permit;
                    let _ = tx.send(reply);
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
        _permit: &runtime::producer::ProducerPermit,
    ) -> Result<AuditReply, AdminFailure> {
        self.write(write).await
    }
    pub(crate) async fn background_cancel(
        &self,
        entry: AuditAccepted,
        outcome: AdminOutcome,
        permit: &runtime::producer::ProducerPermit,
    ) -> Result<(), AdminFailure> {
        let key = entry.key.clone();
        if let AuditReply::Reservation(AuditReservation::Created(_)) = self
            .background_write(AuditWrite::Accept(entry), permit)
            .await?
        {
            self.background_write(
                AuditWrite::Finalize {
                    key,
                    outcome: match outcome {
                        AdminOutcome::Succeeded => AuditOutcome::Succeeded,
                        AdminOutcome::Cancelled => AuditOutcome::Cancelled,
                        AdminOutcome::Conflict => AuditOutcome::Conflict,
                        _ => AuditOutcome::Unknown,
                    },
                    at_ms: now_ms(),
                },
                permit,
            )
            .await?;
        }
        Ok(())
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

fn same_intent(a: &AuditAccepted, b: &AuditAccepted) -> bool {
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
        let permit = producer.try_enter().unwrap();
        assert!(
            audit
                .background_write(AuditWrite::Stats, &permit)
                .await
                .is_err()
        );
        assert_eq!(port.0.load(Ordering::SeqCst), 1);
        gate.transition(1, crate::bootstrap::restart::HostRestartPhase::Capturing);
        assert!(
            audit
                .background_write(AuditWrite::Stats, &permit)
                .await
                .is_err()
        );
        assert_eq!(port.0.load(Ordering::SeqCst), 1);
    }
}
