//! Observe live ownership without mistaking its heartbeat for restart effects.
use event_bus::{EventKind, EventReceiver, OwnershipAction, OwnershipEvent};
use runtime::ownership::{ThreadOwner, ipc};
use std::path::Path;

pub struct State {
    other: String,
    owners: Vec<ThreadOwner>,
}

impl State {
    pub fn new(other: String, owners: Vec<ThreadOwner>) -> Self {
        Self { other, owners }
    }

    pub fn assert_unchanged_except_lease_renewal(&self, after: &Self, renewals: &[OwnershipEvent]) {
        assert_eq!(after.other, self.other);
        assert_owners_unchanged_except_lease_renewal(&self.owners, &after.owners, renewals);
    }
}

/// Only the expiry is heartbeat maintenance, not ownership identity. Keep every
/// other field, including holder, generation, active runs and both hold receipts.
fn without_lease_renewal(owners: &[ThreadOwner]) -> Vec<ThreadOwner> {
    owners
        .iter()
        .cloned()
        .map(|mut owner| {
            owner.lease.expires_at = 0;
            owner
        })
        .collect()
}

fn assert_owners_unchanged_except_lease_renewal(
    before: &[ThreadOwner],
    after: &[ThreadOwner],
    renewals: &[OwnershipEvent],
) {
    assert_eq!(without_lease_renewal(after), without_lease_renewal(before));
    for event in renewals {
        let owner = before
            .iter()
            .find(|owner| owner.thread_id == event.thread_id)
            .expect("unexpected ownership event");
        assert_eq!(
            event.action,
            OwnershipAction::Heartbeat,
            "owner transition was not a heartbeat"
        );
        assert_eq!(
            event.owner_id, owner.lease.owner_id,
            "heartbeat holder changed"
        );
        assert_eq!(event.generation, owner.lease.generation);
    }
    for (previous, current) in before.iter().zip(after) {
        assert!(
            current.lease.expires_at >= previous.lease.expires_at,
            "lease expiration regressed"
        );
        if current.lease.expires_at != previous.lease.expires_at {
            assert!(
                renewals
                    .iter()
                    .any(|event| event.thread_id == previous.thread_id),
                "lease renewal without a matching heartbeat"
            );
            // Check that the exact observed row is a legal heartbeat transition,
            // not just that some unrelated ownership event was delivered.
            let renewed_at = current
                .lease
                .expires_at
                .checked_sub(previous.settings.lease_ms.get())
                .expect("expiry cannot result from a heartbeat");
            let mut renewed = previous.clone();
            renewed.heartbeat(&previous.lease, renewed_at).unwrap();
            assert_eq!(&renewed, current);
        }
    }
}

/// Force the formerly racy interleaving using a real worker renewal. Receiving
/// events (not elapsed time) drives progress; the runner owns hang detection.
pub async fn wait_for_renewal(
    host: &workbench_host::bootstrap::BootstrappedHost,
    before: &State,
    events: &mut EventReceiver,
) -> Vec<OwnershipEvent> {
    assert_eq!(before.owners.len(), 1);
    let expected = &before.owners[0];
    let mut observed = Vec::new();
    loop {
        let EventKind::Ownership(event) = events.recv().await.unwrap().kind else {
            continue;
        };
        let renewed =
            event.thread_id == expected.thread_id && event.action == OwnershipAction::Heartbeat;
        observed.push(event);
        if renewed {
            let owners = host.owner.restart_inventory().unwrap();
            let current = owners
                .iter()
                .find(|owner| owner.thread_id == expected.thread_id)
                .unwrap();
            assert_eq!(current.lease.owner_id, expected.lease.owner_id);
            assert_eq!(current.lease.generation, expected.lease.generation);
            if current.lease.expires_at > expected.lease.expires_at {
                return observed;
            }
        }
    }
}

pub async fn finish_renewals(
    sockets: &Path,
    before: &State,
    events: &mut EventReceiver,
    mut observed: Vec<OwnershipEvent>,
) -> Vec<OwnershipEvent> {
    let owner = &before.owners[0];
    let socket = sockets
        .join("owners")
        .join(format!("{}.sock", owner.lease.owner_id));
    let request = ipc::Request::Attach {
        thread_id: owner.thread_id.clone(),
    };
    // The same worker emits each heartbeat after its DB commit, before serving
    // the next IPC request. Its read-only Attach ACK closes that observation gap
    // even if it was descheduled between commit and emit when we read the row.
    let response = tokio::task::spawn_blocking(move || ipc::request(&socket, &request))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(response, ipc::Response::Status(_)));
    let (pending, lagged) = events.drain_pending_snapshot_counting_lag();
    assert_eq!(lagged, 0, "lost ownership evidence");
    observed.extend(pending.into_iter().filter_map(|event| match event.kind {
        EventKind::Ownership(owner) => Some(owner),
        _ => None,
    }));
    observed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner() -> ThreadOwner {
        ThreadOwner::new(
            "owned".into(),
            runtime::ownership::Lease {
                owner_id: "holder".into(),
                generation: 2,
                expires_at: 20_000,
            },
        )
    }
    fn heartbeat() -> OwnershipEvent {
        OwnershipEvent {
            thread_id: "owned".into(),
            owner_id: "holder".into(),
            generation: 2,
            action: OwnershipAction::Heartbeat,
        }
    }
    fn check(expires_at: u64, events: &[OwnershipEvent]) {
        let previous = owner();
        let mut current = previous.clone();
        current.lease.expires_at = expires_at;
        assert_owners_unchanged_except_lease_renewal(&[previous], &[current], events);
    }
    #[test]
    fn accepts_only_a_witnessed_monotonic_heartbeat() {
        check(20_000, &[]);
        check(21_000, &[heartbeat()]);
    }
    #[test]
    #[should_panic(expected = "lease expiration regressed")]
    fn rejects_expiry_regression_even_with_heartbeat() {
        check(19_000, &[heartbeat()]);
    }
    #[test]
    #[should_panic(expected = "lease renewal without a matching heartbeat")]
    fn rejects_unwitnessed_expiry_change() {
        check(21_000, &[]);
    }
    #[test]
    #[should_panic(expected = "heartbeat holder changed")]
    fn rejects_foreign_heartbeat_as_renewal_evidence() {
        let mut event = heartbeat();
        event.owner_id = "other-holder".into();
        check(21_000, &[event]);
    }
    #[test]
    #[should_panic(expected = "owner transition was not a heartbeat")]
    fn rejects_other_owner_transitions_even_with_unchanged_final_row() {
        let mut event = heartbeat();
        event.action = OwnershipAction::Quiescing;
        check(20_000, &[event]);
    }
    #[test]
    fn normalization_preserves_every_other_owner_field() {
        let previous = owner();
        let mutations: [fn(&mut ThreadOwner); 9] = [
            |o| o.thread_id.push('x'),
            |o| o.lease.owner_id.push('x'),
            |o| o.lease.generation += 1,
            |o| o.state = runtime::ownership::OwnerState::Quiescing,
            |o| o.active_turn = true,
            |o| {
                o.active_runs.insert("run-1".into());
            },
            |o| o.settings.lease_ms = std::num::NonZeroU64::new(1).unwrap(),
            |o| o.handoff_hold = Some(Box::new(hold())),
            |o| o.planned_claim = Some(Box::new(hold())),
        ];
        for mutate in mutations {
            let mut current = previous.clone();
            mutate(&mut current);
            assert_ne!(
                without_lease_renewal(std::slice::from_ref(&previous)),
                without_lease_renewal(&[current]),
            );
        }
    }
    fn hold() -> runtime::ownership::PlannedHold {
        runtime::ownership::PlannedHold {
            transaction_id: "tx".into(),
            capture_id: "capture".into(),
            thread_id: "owned".into(),
            owner_id: "holder".into(),
            generation: 2,
            active_runs: Default::default(),
            parked_runs: Default::default(),
            orphaned: false,
        }
    }
}
