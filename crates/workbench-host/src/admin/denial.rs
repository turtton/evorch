use super::{audit::now_ms, *};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use storage::admin_audit::*;
use workbench_authority::{AuthorityDenial, IngressContext};
use workbench_protocol::{
    envelope::Surface,
    namespaces::lifecycle::{
        AdminAction,
        restart::{AdminAuditDiagnostics, DenialLossSummary},
    },
    wire::Revision,
};
pub const MAX_DENIAL_KEYS: usize = 256;
pub const MAX_DENIAL_QUEUE: usize = 256;
pub const DENIAL_BATCH: usize = 64;
#[derive(Default)]
struct State {
    window: u64,
    keys: Vec<DenialAggregate>,
    overflow: Option<DenialAggregate>,
    queue: VecDeque<DenialAggregate>,
    backlog_overflow: Option<DenialAggregate>,
    /// Rows storage explicitly dropped, or whose persistence result is unknown.
    persistence_failures: u64,
    loss: Option<DenialLossSummary>,
}
#[derive(Default)]
pub(crate) struct Denials(Mutex<State>);
impl Denials {
    pub fn record(&self, ctx: &IngressContext, action: AdminAction, reason: AuthorityDenial) {
        self.record_reason(
            ctx,
            action,
            match reason {
                AuthorityDenial::Unauthenticated => DenialReason::Unauthenticated,
                AuthorityDenial::NonHuman => DenialReason::NonHuman,
                AuthorityDenial::SurfaceMismatch => DenialReason::SurfaceMismatch,
                AuthorityDenial::Expired => DenialReason::Expired,
            },
        );
    }
    pub fn record_reason(&self, ctx: &IngressContext, action: AdminAction, reason: DenialReason) {
        self.record_at(ctx, action, reason, now_ms());
    }
    fn record_at(&self, ctx: &IngressContext, action: AdminAction, reason: DenialReason, now: u64) {
        let window = now / 60_000 * 60_000;
        let principal = ctx.authority().principal().map(|p| AuditPrincipal {
            method: p.method().to_owned(),
            subject: p.subject().to_owned(),
            binding: p.binding().to_owned(),
        });
        let key = DenialKey::Caller {
            principal,
            surface: surface(ctx),
            operation: denial_operation(action),
            reason,
        };
        let mut state = self.0.lock().unwrap();
        if state.window != window {
            Self::rotate(&mut state);
            state.window = window;
        }
        if let Some(row) = state.keys.iter_mut().find(|r| r.key == key) {
            row.last_ms = now;
            row.count = row.count.saturating_add(1);
            return;
        }
        let row = DenialAggregate {
            key,
            window_start_ms: window,
            first_ms: now,
            last_ms: now,
            count: 1,
        };
        if state.keys.len() < MAX_DENIAL_KEYS {
            state.keys.push(row);
        } else if let Some(row) = &mut state.overflow {
            row.last_ms = now;
            row.count = row.count.saturating_add(1);
        } else {
            state.overflow = Some(DenialAggregate {
                key: DenialKey::Overflow,
                ..row
            });
        }
    }
    fn enqueue(state: &mut State, row: DenialAggregate) {
        if state.queue.len() < MAX_DENIAL_QUEUE {
            state.queue.push_back(row);
        } else if let Some(overflow) = &mut state.backlog_overflow {
            if overflow.window_start_ms != row.window_start_ms {
                state.persistence_failures = state.persistence_failures.saturating_add(1);
                if let Some(loss) = &mut state.loss {
                    loss.count.0 = loss.count.0.saturating_add(row.count);
                    loss.first_ms.0 = loss.first_ms.0.min(row.first_ms);
                    loss.last_ms.0 = loss.last_ms.0.max(row.last_ms);
                } else {
                    state.loss = Some(DenialLossSummary {
                        count: Revision(row.count),
                        first_ms: Revision(row.first_ms),
                        last_ms: Revision(row.last_ms),
                    });
                }
                return;
            }
            overflow.first_ms = overflow.first_ms.min(row.first_ms);
            overflow.last_ms = overflow.last_ms.max(row.last_ms);
            overflow.count = overflow.count.saturating_add(row.count);
        } else {
            state.backlog_overflow = Some(DenialAggregate {
                key: DenialKey::Overflow,
                ..row
            });
        }
    }
    fn rotate(state: &mut State) {
        let rows: Vec<_> = state.keys.drain(..).chain(state.overflow.take()).collect();
        for row in rows {
            Self::enqueue(state, row);
        }
    }
    fn batch(&self) -> Vec<DenialAggregate> {
        let mut state = self.0.lock().unwrap();
        if now_ms() / 60_000 != state.window / 60_000 {
            Self::rotate(&mut state);
            state.window = (now_ms() / 60_000) * 60_000;
        }
        // Overflow gets priority so a permanently busy queue cannot starve its count.
        let mut rows: Vec<_> = state.backlog_overflow.take().into_iter().collect();
        let n = state.queue.len().min(DENIAL_BATCH - rows.len());
        rows.extend(state.queue.drain(..n));
        rows
    }
    pub fn diagnostics(&self) -> AdminAuditDiagnostics {
        let state = self.0.lock().unwrap();
        AdminAuditDiagnostics {
            denial_persistence_failures: Revision(state.persistence_failures),
            denial_loss: state.loss.clone(),
        }
    }
    pub fn persistence_failures(&self) -> u64 {
        self.0.lock().unwrap().persistence_failures
    }
    fn account_persistence(&self, submitted: usize, result: &Result<AuditReply, AdminFailure>) {
        let lost = match result {
            Ok(AuditReply::Denials { stored, dropped })
                if stored.checked_add(*dropped) == Some(submitted) =>
            {
                *dropped
            }
            _ => submitted,
        };
        let mut state = self.0.lock().unwrap();
        state.persistence_failures = state.persistence_failures.saturating_add(lost as u64);
    }
    async fn flush(&self, audit: &AdminAudit, producers: &runtime::producer::ProducerGate) {
        audit.reap_pending(producers).await;
        if !producers.is_open() {
            return;
        }
        let rows = self.batch();
        if !rows.is_empty() {
            let count = rows.len();
            let result = audit
                .background_write(
                    AuditWrite::Denials {
                        rows: rows.clone(),
                        now_ms: now_ms(),
                    },
                    producers,
                )
                .await;
            if result
                .as_ref()
                .is_err_and(|e| e.code == workbench_protocol::errors::ErrorCode::Busy)
            {
                let mut state = self.0.lock().unwrap();
                for row in rows {
                    Self::enqueue(&mut state, row);
                }
            } else {
                self.account_persistence(count, &result);
            }
        }
        // This is a new admission, not authority borrowed from the denial ACK.
        let _ = audit
            .background_write(AuditWrite::Cleanup { now_ms: now_ms() }, producers)
            .await;
    }
    pub async fn run(
        self: Arc<Self>,
        audit: Arc<AdminAudit>,
        producers: runtime::producer::ProducerGate,
        mut stop: tokio::sync::oneshot::Receiver<()>,
    ) {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! { _ = &mut stop => return, _ = interval.tick() => {}, _ = audit.pending_changed() => {} }
            tokio::select! { _ = &mut stop => return, _ = self.flush(&audit, &producers) => {} }
        }
    }
}
pub(crate) fn surface(ctx: &IngressContext) -> AuditSurface {
    match ctx.provenance().surface {
        Surface::Desktop => AuditSurface::Desktop,
        Surface::Cli => AuditSurface::Cli,
        Surface::Web => AuditSurface::Web,
        _ => AuditSurface::Unknown,
    }
}
fn denial_operation(a: AdminAction) -> DenialOperation {
    match a {
        AdminAction::Targets => DenialOperation::Targets,
        AdminAction::Request => DenialOperation::RestartRequest,
        AdminAction::Cancel => DenialOperation::Cancel,
        AdminAction::Status => DenialOperation::Status,
        AdminAction::Interrupt => DenialOperation::Interrupt,
        AdminAction::Quarantine => DenialOperation::Quarantine,
        AdminAction::Discard => DenialOperation::Discard,
        AdminAction::Release => DenialOperation::Release,
        AdminAction::Resume => DenialOperation::Resume,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_authority::test_support;
    use workbench_protocol::{envelope::Provenance, ids::ClientId};
    #[test]
    fn anonymous_claims_do_not_create_keys_and_boundary_rolls_once() {
        let d = Denials::default();
        for n in 0..10_000 {
            let c = IngressContext::unverified(Provenance::human(
                Surface::Web,
                ClientId::new(format!("claim-{n}")).unwrap(),
            ));
            d.record_at(
                &c,
                AdminAction::Request,
                DenialReason::Unauthenticated,
                60_000 + n,
            );
        }
        {
            let s = d.0.lock().unwrap();
            assert_eq!(s.keys.len(), 1);
            assert_eq!(s.keys[0].count, 10_000);
            assert_eq!(s.keys[0].first_ms, 60_000);
            assert_eq!(s.keys[0].last_ms, 69_999);
            assert!(matches!(
                s.keys[0].key,
                DenialKey::Caller {
                    principal: None,
                    ..
                }
            ));
        }
        let c = IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("last").unwrap(),
        ));
        d.record_at(
            &c,
            AdminAction::Request,
            DenialReason::Unauthenticated,
            120_000,
        );
        let s = d.0.lock().unwrap();
        assert_eq!(s.queue.len(), 1);
        assert_eq!(s.keys.len(), 1);
        assert_eq!(s.keys[0].count, 1);
    }
    #[test]
    fn verified_high_cardinality_is_bounded_with_overflow_and_bounded_backlog() {
        let d = Denials::default();
        let mut leases = Vec::new();
        for window in 1..=4 {
            for n in 0..400 {
                let (p, lease) = test_support::web(
                    &format!("subject-{n}"),
                    tokio::time::Instant::now() + std::time::Duration::from_secs(60),
                );
                leases.push(lease);
                let c = IngressContext::new(
                    Provenance::human(Surface::Web, ClientId::new("web").unwrap()),
                    workbench_authority::TransportAuthority::from_web_context(Some(&p)),
                );
                d.record_at(
                    &c,
                    AdminAction::Request,
                    DenialReason::Capacity,
                    window * 60_000,
                );
                let s = d.0.lock().unwrap();
                assert!(s.keys.len() <= MAX_DENIAL_KEYS);
                assert!(s.queue.len() <= MAX_DENIAL_QUEUE);
            }
            let s = d.0.lock().unwrap();
            assert_eq!(s.keys.len(), 256);
            assert_eq!(s.overflow.as_ref().unwrap().count, 144);
        }
        let s = d.0.lock().unwrap();
        assert_eq!(s.queue.len(), 256);
        let total = s
            .keys
            .iter()
            .chain(s.overflow.iter())
            .chain(s.queue.iter())
            .chain(s.backlog_overflow.iter())
            .map(|r| r.count)
            .sum::<u64>();
        assert_eq!(total + s.loss.as_ref().map_or(0, |l| l.count.0), 1600);
        assert!(s.backlog_overflow.as_ref().unwrap().count > 0);
    }
    #[test]
    fn persistence_errors_and_explicit_dropped_rows_are_visible() {
        let d = Denials::default();
        d.account_persistence(
            64,
            &Ok(AuditReply::Denials {
                stored: 60,
                dropped: 4,
            }),
        );
        assert_eq!(d.persistence_failures(), 4);
        d.account_persistence(3, &Err(super::super::unknown()));
        assert_eq!(d.persistence_failures(), 7);
        d.account_persistence(
            1,
            &Ok(AuditReply::Denials {
                stored: 1,
                dropped: 0,
            }),
        );
        assert_eq!(d.persistence_failures(), 7);
    }
    #[tokio::test]
    async fn pause_between_denial_ack_and_cleanup_forbids_the_new_command() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        struct Port {
            count: AtomicUsize,
            tx: tokio::sync::mpsc::UnboundedSender<(AuditWrite, AuditReplyCallback)>,
        }
        impl AuditPort for Port {
            fn submit(
                &self,
                write: AuditWrite,
                reply: AuditReplyCallback,
            ) -> Result<(), storage::StorageError> {
                self.count.fetch_add(1, Ordering::SeqCst);
                self.tx
                    .send((write, reply))
                    .map_err(|_| storage::StorageError::Io("closed".into()))
            }
        }
        let d = Arc::new(Denials::default());
        let ctx = IngressContext::unverified(Provenance::human(
            Surface::Web,
            ClientId::new("web").unwrap(),
        ));
        d.record_at(
            &ctx,
            AdminAction::Request,
            DenialReason::Unauthenticated,
            now_ms().saturating_sub(60_000),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let port = Arc::new(Port {
            count: AtomicUsize::new(0),
            tx,
        });
        let gate = crate::bootstrap::restart::HostRestartGate::default();
        let audit = Arc::new(AdminAudit::new(port.clone(), gate.clone()));
        let producers = runtime::producer::ProducerGate::default();
        let job = tokio::spawn({
            let d = d.clone();
            let audit = audit.clone();
            let producers = producers.clone();
            async move { d.flush(&audit, &producers).await }
        });
        let (write, reply) = rx.recv().await.unwrap();
        let AuditWrite::Denials { rows, .. } = write else {
            panic!("denial flush expected")
        };
        let _pause = producers.pause();
        // Pause alone must reject the next command, even before the host gate
        // transitions. The separate audit gate test covers Preparing/Sealing.
        reply(Ok(AuditReply::Denials {
            stored: rows.len(),
            dropped: 0,
        }));
        job.await.unwrap();
        assert_eq!(port.count.load(Ordering::SeqCst), 1);
        assert_eq!(d.persistence_failures(), 0);
    }
    #[test]
    fn multi_window_saturation_has_zero_invalid_rows_and_exact_observable_loss() {
        let d = Denials::default();
        let row = |window: u64, count| DenialAggregate {
            key: DenialKey::Overflow,
            window_start_ms: window * DENIAL_WINDOW_MS,
            first_ms: window * DENIAL_WINDOW_MS + 1,
            last_ms: window * DENIAL_WINDOW_MS + 2,
            count,
        };
        {
            let mut state = d.0.lock().unwrap();
            for _ in 0..MAX_DENIAL_QUEUE {
                Denials::enqueue(&mut state, row(1, 1));
            }
            Denials::enqueue(&mut state, row(1, 2));
            Denials::enqueue(&mut state, row(1, 3));
            Denials::enqueue(&mut state, row(2, 7));
            Denials::enqueue(&mut state, row(3, 11));
            assert_eq!(state.queue.len(), 256);
            assert_eq!(state.backlog_overflow.as_ref().unwrap().count, 5);
        }
        let diagnostics = d.diagnostics();
        let loss = diagnostics.denial_loss.unwrap();
        assert_eq!(loss.count, Revision(18));
        assert_eq!(
            (loss.first_ms, loss.last_ms),
            (
                Revision(2 * DENIAL_WINDOW_MS + 1),
                Revision(3 * DENIAL_WINDOW_MS + 2)
            )
        );
        assert_eq!(diagnostics.denial_persistence_failures, Revision(2));
        let dir = tempfile::tempdir().unwrap();
        let storage = storage::Storage::open(storage::StorageConfig {
            db_path: dir.path().join("overflow.db"),
            ..Default::default()
        })
        .unwrap();
        let mut persisted_count = 0;
        loop {
            let rows = d.batch();
            if rows.is_empty() {
                break;
            }
            persisted_count += rows.iter().map(|r| r.count).sum::<u64>();
            let expected = rows.len();
            assert_eq!(
                storage
                    .handle()
                    .admin_audit(AuditWrite::Denials {
                        rows,
                        now_ms: 4 * DENIAL_WINDOW_MS
                    })
                    .unwrap(),
                AuditReply::Denials {
                    stored: expected,
                    dropped: 0
                }
            );
        }
        assert_eq!(persisted_count + loss.count.0, 256 + 2 + 3 + 7 + 11);
        assert_eq!(
            Denials::default().diagnostics(),
            AdminAuditDiagnostics::default(),
            "process-local diagnostics reset"
        );
    }
}
