use super::{audit::now_ms, *};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};
use storage::admin_audit::*;
use workbench_authority::{AuthorityDenial, IngressContext};
use workbench_protocol::{envelope::Surface, namespaces::lifecycle::AdminAction};
pub const MAX_DENIAL_KEYS: usize = 256;
pub const MAX_DENIAL_QUEUE: usize = 512;
pub const DENIAL_BATCH: usize = 64;
#[derive(Default)]
struct State {
    window: u64,
    keys: Vec<DenialAggregate>,
    overflow: Option<DenialAggregate>,
    queue: VecDeque<DenialAggregate>,
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
    fn rotate(state: &mut State) {
        for row in state.keys.drain(..).chain(state.overflow.take()) {
            if state.queue.len() < MAX_DENIAL_QUEUE {
                state.queue.push_back(row);
            }
        }
    }
    fn batch(&self) -> Vec<DenialAggregate> {
        let mut state = self.0.lock().unwrap();
        if now_ms() / 60_000 != state.window / 60_000 {
            Self::rotate(&mut state);
            state.window = (now_ms() / 60_000) * 60_000;
        }
        let n = state.queue.len().min(DENIAL_BATCH);
        state.queue.drain(..n).collect()
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
            tokio::select! {_=&mut stop=>return,_=interval.tick()=>{}}
            let Some(_permit) = producers.try_enter() else {
                continue;
            };
            let rows = self.batch();
            if !rows.is_empty() {
                let _ = audit
                    .background_write(
                        AuditWrite::Denials {
                            rows,
                            now_ms: now_ms(),
                        },
                        &_permit,
                    )
                    .await;
            }
            let _ = audit
                .background_write(AuditWrite::Cleanup { now_ms: now_ms() }, &_permit)
                .await;
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
        assert_eq!(d.0.lock().unwrap().queue.len(), 512);
    }
}
