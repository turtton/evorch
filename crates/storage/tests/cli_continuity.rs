//! 5b-D storage contracts: the binding, the fixed deadlines, the single redemption, the
//! permanent end of a grant, and what survives a reload. Time is a value the test hands
//! in: nothing here sleeps or reads a clock.
use rusqlite::Connection;
use storage::recovery::cli_continuity::*;
use storage::recovery::restart::*;
use storage::{Database, Storage, StorageConfig, StorageHandle};

const MARKER: &[u8] = b"PLAINTEXT-TOKEN-MARKER-0123456789-abcdefg";
const T0: u64 = 1_000_000;

/// The virtual clock: only the test moves it, forwards or backwards.
struct Clock(u64);
impl Clock {
    fn at(&self, offset_ms: u64) -> u64 {
        self.0 + offset_ms
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    config: StorageConfig,
    storage: Option<Storage>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = StorageConfig {
            db_path: dir.path().join("continuity.db"),
            ..Default::default()
        };
        let storage = Some(Storage::open(config.clone()).unwrap());
        let fixture = Self {
            _dir: dir,
            config,
            storage,
        };
        fixture.restart("r1");
        fixture
    }
    fn handle(&self) -> StorageHandle {
        self.storage.as_ref().unwrap().handle()
    }
    fn sql(&self) -> Connection {
        Connection::open(&self.config.db_path).unwrap()
    }
    /// Closes the writer and opens everything again, as a process restart does.
    fn reload(&mut self) {
        drop(self.storage.take());
        self.storage = Some(Storage::open(self.config.clone()).unwrap());
    }
    /// A durable restart transaction `id` whose capture is `capture-<id>`.
    fn restart(&self, id: &str) -> u64 {
        let metadata = RestartMetadata {
            restart_id: id.into(),
            capture_id: capture(id),
            config_identity: "config".into(),
            old_epoch: 1,
            successor_epoch: 2,
            old_build: "old".into(),
            successor_build: "new".into(),
            target_identity: "target".into(),
            handoff_version: 1,
            snapshot_version: 1,
            storage_version: 72,
            old_builtin_tool_names: Default::default(),
            old_compiled_catalog_fingerprint: None,
            new_compiled_catalog_fingerprint: None,
            old_effective_tools_fingerprint: None,
            new_effective_tools_fingerprint: None,
        };
        self.handle()
            .mutate_restart(id.into(), None, RestartMutation::Create(Box::new(metadata)))
            .unwrap()
            .revision
    }
    fn issue(
        &self,
        invocation: &str,
        restart: &str,
        seed: u8,
        now: u64,
    ) -> ContinuityReport<GrantView> {
        self.handle()
            .issue_cli_continuity(binding(invocation, restart), token(seed), now)
            .unwrap()
    }
    fn ready(&self, restart: &str, now: u64) -> ContinuityReport<HandoffReady> {
        self.handle()
            .mark_cli_handoff_ready(restart.into(), capture(restart), now)
            .unwrap()
    }
    fn redeem(
        &self,
        invocation: &str,
        restart: &str,
        seed: u8,
        now: u64,
    ) -> ContinuityReport<GrantView> {
        self.handle()
            .redeem_cli_continuity(binding(invocation, restart), token(seed), now)
            .unwrap()
    }
    fn grant(&self, invocation: &str, restart: &str) -> Option<GrantView> {
        self.handle()
            .cli_continuity_grant(invocation.into(), restart.into())
            .unwrap()
    }
    fn state(&self, invocation: &str, restart: &str) -> GrantState {
        self.grant(invocation, restart).unwrap().state
    }
}

fn capture(restart: &str) -> String {
    format!("capture-{restart}")
}
fn binding(invocation: &str, restart: &str) -> ContinuityBinding {
    ContinuityBinding {
        invocation_id: invocation.into(),
        restart_id: restart.into(),
        capture_id: capture(restart),
    }
}
fn token(seed: u8) -> ContinuityToken {
    ContinuityToken::new([seed; 32]).unwrap()
}

#[test]
fn a_grant_binds_the_handoff_and_fixes_both_deadlines() {
    let f = Fixture::new();
    let clock = Clock(T0);
    let issued = f.issue("inv-a", "r1", 1, clock.at(0)).unwrap();
    assert_eq!(issued.state, GrantState::Issued);
    assert_eq!(issued.binding, binding("inv-a", "r1"));
    assert_eq!(issued.ready_deadline_ms, T0 + RESTART_TO_READY_MS);
    assert_eq!(
        (issued.ready_at_ms, issued.reconnect_deadline_ms),
        (None, None)
    );
    assert_eq!(RESTART_TO_READY_MS, 120_000);
    assert_eq!(READY_TO_RECONNECT_MS, 30_000);

    let ready = f.ready("r1", clock.at(40_000)).unwrap();
    assert_eq!(ready.ready, vec![binding("inv-a", "r1")]);
    let view = f.grant("inv-a", "r1").unwrap();
    assert_eq!(view.state, GrantState::Ready);
    assert_eq!(view.ready_at_ms, Some(T0 + 40_000));
    assert_eq!(
        view.reconnect_deadline_ms,
        Some(T0 + 40_000 + READY_TO_RECONNECT_MS)
    );
    assert_eq!(view.ready_deadline_ms, T0 + RESTART_TO_READY_MS);
}

#[test]
fn redemption_consumes_the_grant_exactly_once() {
    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", T0 + 10).unwrap();
    let done = f.redeem("inv-a", "r1", 1, T0 + 20).unwrap();
    assert_eq!(done.state, GrantState::Redeemed);
    assert_eq!(done.ended_at_ms, Some(T0 + 20));
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 21),
        Err(ContinuityRefusal::AlreadyRedeemed)
    );
    assert_eq!(
        f.handle()
            .revoke_cli_continuity(
                RevokeScope::Grant {
                    invocation_id: "inv-a".into(),
                    restart_id: "r1".into()
                },
                T0 + 22
            )
            .unwrap(),
        Err(ContinuityRefusal::AlreadyRedeemed)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Redeemed);
}

#[test]
fn nothing_is_redeemed_before_ready_and_the_wait_costs_nothing() {
    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 5),
        Err(ContinuityRefusal::NotReady)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Issued);
    f.ready("r1", T0 + 6).unwrap();
    assert!(f.redeem("inv-a", "r1", 1, T0 + 7).is_ok());
}

#[test]
fn another_invocation_cannot_use_a_token_or_a_grant() {
    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.issue("inv-b", "r1", 2, T0).unwrap();
    f.ready("r1", T0 + 1).unwrap();
    // inv-b presents inv-a's token, and the reverse: the digest is bound to the invocation.
    assert_eq!(
        f.redeem("inv-b", "r1", 1, T0 + 2),
        Err(ContinuityRefusal::TokenMismatch)
    );
    assert_eq!(
        f.redeem("inv-a", "r1", 2, T0 + 2),
        Err(ContinuityRefusal::TokenMismatch)
    );
    // An invocation that holds no grant has nothing to redeem, with any token.
    assert_eq!(
        f.redeem("inv-c", "r1", 1, T0 + 2),
        Err(ContinuityRefusal::NotFound)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Ready);
    assert_eq!(f.state("inv-b", "r1"), GrantState::Ready);
    assert!(f.redeem("inv-a", "r1", 1, T0 + 3).is_ok());
    assert_eq!(f.state("inv-b", "r1"), GrantState::Ready);
}

#[test]
fn another_handoff_cannot_use_a_token_or_a_capture() {
    let f = Fixture::new();
    f.restart("r2");
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.issue("inv-a", "r2", 2, T0).unwrap();
    f.ready("r1", T0 + 1).unwrap();
    f.ready("r2", T0 + 1).unwrap();
    // The same invocation, the token of the other handoff.
    assert_eq!(
        f.redeem("inv-a", "r2", 1, T0 + 2),
        Err(ContinuityRefusal::TokenMismatch)
    );
    assert_eq!(
        f.redeem("inv-a", "r1", 2, T0 + 2),
        Err(ContinuityRefusal::TokenMismatch)
    );
    // The right restart under the capture of another one.
    let wrong_capture = ContinuityBinding {
        capture_id: capture("r2"),
        ..binding("inv-a", "r1")
    };
    assert_eq!(
        f.handle()
            .redeem_cli_continuity(wrong_capture, token(1), T0 + 2)
            .unwrap(),
        Err(ContinuityRefusal::HandoffMismatch)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Ready);
    assert_eq!(f.state("inv-a", "r2"), GrantState::Ready);
}

#[test]
fn a_grant_needs_a_live_restart_transaction_with_the_same_capture() {
    let f = Fixture::new();
    assert_eq!(
        f.issue("inv-a", "missing", 1, T0),
        Err(ContinuityRefusal::HandoffNotFound)
    );
    let wrong = ContinuityBinding {
        capture_id: "other".into(),
        ..binding("inv-a", "r1")
    };
    assert_eq!(
        f.handle()
            .issue_cli_continuity(wrong, token(1), T0)
            .unwrap(),
        Err(ContinuityRefusal::HandoffMismatch)
    );
    assert_eq!(
        f.handle()
            .mark_cli_handoff_ready("r1".into(), "other".into(), T0)
            .unwrap(),
        Err(ContinuityRefusal::HandoffMismatch)
    );
    let revision = f.restart("r3");
    f.handle()
        .invalidate_restart("r3".into(), revision, "test".into())
        .unwrap();
    assert_eq!(
        f.issue("inv-a", "r3", 1, T0),
        Err(ContinuityRefusal::HandoffEnded)
    );
    assert!(f.grant("inv-a", "r3").is_none() && f.grant("inv-a", "r1").is_none());
}

#[test]
fn the_ready_deadline_is_exclusive_and_a_late_ready_revives_nothing() {
    let f = Fixture::new();
    f.issue("early", "r1", 1, T0).unwrap();
    let on_time = f.ready("r1", T0 + RESTART_TO_READY_MS - 1).unwrap();
    assert_eq!(on_time.ready, vec![binding("early", "r1")]);

    let f = Fixture::new();
    f.issue("late", "r1", 1, T0).unwrap();
    let late = f.ready("r1", T0 + RESTART_TO_READY_MS).unwrap();
    assert!(late.ready.is_empty());
    assert_eq!(late.expired, vec![binding("late", "r1")]);
    let view = f.grant("late", "r1").unwrap();
    assert_eq!((view.state, view.ready_at_ms), (GrantState::Expired, None));
    assert_eq!(view.ended_at_ms, Some(T0 + RESTART_TO_READY_MS));
    // Ready again, and a perfect token, change nothing.
    assert!(
        f.ready("r1", T0 + RESTART_TO_READY_MS + 1)
            .unwrap()
            .ready
            .is_empty()
    );
    assert_eq!(
        f.redeem("late", "r1", 1, T0 + RESTART_TO_READY_MS + 2),
        Err(ContinuityRefusal::Expired)
    );
}

#[test]
fn the_reconnect_deadline_is_exclusive() {
    let ready_at = T0 + 40_000;
    let deadline = ready_at + READY_TO_RECONNECT_MS;

    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", ready_at).unwrap();
    assert_eq!(
        f.redeem("inv-a", "r1", 1, deadline - 1).unwrap().state,
        GrantState::Redeemed
    );

    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", ready_at).unwrap();
    assert_eq!(
        f.redeem("inv-a", "r1", 1, deadline),
        Err(ContinuityRefusal::Expired)
    );
    let view = f.grant("inv-a", "r1").unwrap();
    assert_eq!(view.state, GrantState::Expired);
    assert_eq!(view.ended_at_ms, Some(deadline));
    assert_eq!(
        f.redeem("inv-a", "r1", 1, deadline + 1),
        Err(ContinuityRefusal::Expired)
    );
}

#[test]
fn a_second_issue_never_restarts_a_deadline() {
    let f = Fixture::new();
    let first = f.issue("inv-a", "r1", 1, T0).unwrap();
    // The same token, another token, and a later time: every repeat is refused.
    assert_eq!(
        f.issue("inv-a", "r1", 1, T0 + 5_000),
        Err(ContinuityRefusal::AlreadyIssued)
    );
    assert_eq!(
        f.issue("inv-a", "r1", 9, T0 + 6_000),
        Err(ContinuityRefusal::AlreadyIssued)
    );
    assert_eq!(f.grant("inv-a", "r1").unwrap(), first);
    // Not even after the grant ended.
    f.handle()
        .expire_due_cli_continuity(T0 + RESTART_TO_READY_MS)
        .unwrap()
        .unwrap();
    assert_eq!(f.state("inv-a", "r1"), GrantState::Expired);
    assert_eq!(
        f.issue("inv-a", "r1", 1, T0 + RESTART_TO_READY_MS + 1),
        Err(ContinuityRefusal::AlreadyIssued)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Expired);
}

#[test]
fn ready_twice_does_not_move_the_reconnect_deadline() {
    let f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", T0 + 10_000).unwrap();
    let before = f.grant("inv-a", "r1").unwrap();
    let again = f.ready("r1", T0 + 25_000).unwrap();
    assert!(again.ready.is_empty() && again.expired.is_empty());
    assert_eq!(f.grant("inv-a", "r1").unwrap(), before);
    assert_eq!(
        before.reconnect_deadline_ms,
        Some(T0 + 10_000 + READY_TO_RECONNECT_MS)
    );
    // The original deadline is the one that decides.
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 10_000 + READY_TO_RECONNECT_MS),
        Err(ContinuityRefusal::Expired)
    );
}

#[test]
fn a_raw_write_cannot_reset_extend_or_revive() {
    let f = Fixture::new();
    f.issue("live", "r1", 1, T0).unwrap();
    f.issue("gone", "r1", 2, T0).unwrap();
    f.issue("done", "r1", 3, T0).unwrap();
    f.restart("r2");
    f.issue("dead", "r2", 4, T0).unwrap();
    f.ready("r1", T0 + 1).unwrap();
    f.redeem("done", "r1", 3, T0 + 2).unwrap();
    f.handle()
        .revoke_cli_continuity(
            RevokeScope::Grant {
                invocation_id: "dead".into(),
                restart_id: "r2".into(),
            },
            T0 + 3,
        )
        .unwrap()
        .unwrap();
    f.handle()
        .revoke_cli_continuity(
            RevokeScope::Grant {
                invocation_id: "gone".into(),
                restart_id: "r1".into(),
            },
            T0 + 4,
        )
        .unwrap()
        .unwrap();
    let sql = f.sql();
    let rejected = |statement: &str| {
        let error = sql.execute(statement, []).unwrap_err();
        assert!(
            matches!(
                &error,
                rusqlite::Error::SqliteFailure(failure, _)
                    if failure.code == rusqlite::ErrorCode::ConstraintViolation
            ),
            "{statement}: {error}"
        );
    };
    let live = "WHERE invocation_id='live'";
    rejected(&format!(
        "UPDATE cli_continuity_grants SET reconnect_deadline_ms=reconnect_deadline_ms+1 {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET ready_at_ms=ready_at_ms+1,reconnect_deadline_ms=reconnect_deadline_ms+1 {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET ready_deadline_ms=ready_deadline_ms+1 {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET issued_at_ms=issued_at_ms+1,ready_deadline_ms=ready_deadline_ms+1 {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET token_digest=zeroblob(32) {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET invocation_id='x' {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET capture_id='x' {live}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET state='issued' {live}"
    ));
    rejected(&format!("DELETE FROM cli_continuity_grants {live}"));
    // An ended grant never goes back to a live state, from any way of ending.
    for (name, back) in [("dead", "issued"), ("gone", "ready"), ("done", "ready")] {
        rejected(&format!(
            "UPDATE cli_continuity_grants SET state='{back}',ended_at_ms=NULL WHERE invocation_id='{name}'"
        ));
    }
    let gone = "WHERE invocation_id='gone'";
    rejected(&format!(
        "UPDATE cli_continuity_grants SET state='issued',ended_at_ms=NULL {gone}"
    ));
    rejected(&format!(
        "UPDATE cli_continuity_grants SET state='ready',ended_at_ms=NULL,ready_at_ms=5,reconnect_deadline_ms=30005 {gone}"
    ));
    // A new row cannot carry a deadline the constants do not give.
    rejected(
        "INSERT INTO cli_continuity_grants(invocation_id,restart_id,capture_id,token_digest,issued_at_ms,ready_deadline_ms,state) VALUES('i','r1','capture-r1',zeroblob(32),0,1000000,'issued')",
    );
    rejected(
        "INSERT INTO cli_continuity_grants(invocation_id,restart_id,capture_id,token_digest,issued_at_ms,ready_deadline_ms,state,ready_at_ms,reconnect_deadline_ms) VALUES('i','r1','capture-r1',zeroblob(32),0,120000,'ready',10,10000000)",
    );
    // The clock does not move back and is never removed.
    rejected("UPDATE cli_continuity_clock SET high_water_ms=0");
    rejected("DELETE FROM cli_continuity_clock");
    assert_eq!(f.state("live", "r1"), GrantState::Ready);
    assert_eq!(f.state("gone", "r1"), GrantState::Revoked);
}

#[test]
fn a_clock_that_moves_back_is_refused_and_ends_live_grants_once() {
    let f = Fixture::new();
    let clock = Clock(T0);
    f.issue("inv-a", "r1", 1, clock.at(10_000)).unwrap();
    let high_water = clock.at(10_000);
    // Before the time the writer has seen: refused, whatever the call, and nothing new is
    // created.
    assert_eq!(
        f.issue("inv-b", "r1", 2, clock.at(9_999)),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert!(f.grant("inv-b", "r1").is_none());
    // The refusal ended the live grant for good, stamped with the time already observed.
    let ended = f.grant("inv-a", "r1").unwrap();
    assert_eq!(ended.state, GrantState::Expired);
    assert_eq!(ended.ended_at_ms, Some(high_water));
    assert_eq!(
        f.ready("r1", clock.at(9_999)),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert_eq!(
        f.redeem("inv-a", "r1", 1, clock.at(0)),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert_eq!(
        f.handle().expire_due_cli_continuity(clock.at(1)).unwrap(),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert_eq!(
        f.handle()
            .revoke_cli_continuity(
                RevokeScope::Handoff {
                    restart_id: "r1".into()
                },
                clock.at(1)
            )
            .unwrap(),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert_eq!(f.grant("inv-a", "r1").unwrap(), ended);
}

#[test]
fn an_expired_grant_stays_expired_when_the_clock_goes_back() {
    let mut f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", T0 + 1_000).unwrap();
    let late = T0 + 1_000 + READY_TO_RECONNECT_MS + 5;
    assert_eq!(
        f.redeem("inv-a", "r1", 1, late),
        Err(ContinuityRefusal::Expired)
    );
    // Back to a time inside the window: refused, not redeemed. The high-water mark
    // survives a reload, so a restarted writer refuses it too.
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 2_000),
        Err(ContinuityRefusal::ClockRegressed)
    );
    f.reload();
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 2_000),
        Err(ContinuityRefusal::ClockRegressed)
    );
    assert_eq!(f.state("inv-a", "r1"), GrantState::Expired);
    // And a time past the window cannot revive it either.
    assert_eq!(
        f.redeem("inv-a", "r1", 1, late + 1),
        Err(ContinuityRefusal::Expired)
    );
}

#[test]
fn catching_up_after_a_regression_cannot_redeem_past_the_deadline() {
    let mut f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.issue("inv-b", "r1", 2, T0).unwrap();
    f.ready("r1", T0 + 1_000).unwrap();
    // Time was seen up to the deadline minus one.
    let seen = T0 + 1_000 + READY_TO_RECONNECT_MS - 1;
    f.handle().expire_due_cli_continuity(seen).unwrap().unwrap();
    assert_eq!(f.state("inv-a", "r1"), GrantState::Ready);
    // The clock is set back by a minute and the regression is observed ...
    assert_eq!(
        f.redeem("inv-a", "r1", 1, seen - 60_000),
        Err(ContinuityRefusal::ClockRegressed)
    );
    // ... which ended every live grant, so a reload and a clock that catches up to the
    // old high-water mark (real time elapsed: far past the window) redeem nothing.
    f.reload();
    for (name, seed) in [("inv-a", 1), ("inv-b", 2)] {
        assert_eq!(f.state(name, "r1"), GrantState::Expired);
        assert_eq!(
            f.redeem(name, "r1", seed, seen),
            Err(ContinuityRefusal::Expired)
        );
    }
    for (name, seed) in [("inv-a", 1), ("inv-b", 2)] {
        assert_eq!(
            f.redeem(name, "r1", seed, seen + 60_000),
            Err(ContinuityRefusal::Expired)
        );
    }
}

#[test]
fn state_and_deadlines_survive_a_reload_without_extension() {
    let mut f = Fixture::new();
    f.issue("redeemed", "r1", 1, T0).unwrap();
    f.issue("waiting", "r1", 2, T0).unwrap();
    f.issue("revoked", "r1", 3, T0).unwrap();
    f.ready("r1", T0 + 100).unwrap();
    f.redeem("redeemed", "r1", 1, T0 + 200).unwrap();
    f.handle()
        .revoke_cli_continuity(
            RevokeScope::Invocation {
                invocation_id: "revoked".into(),
            },
            T0 + 300,
        )
        .unwrap()
        .unwrap();
    let names = ["redeemed", "waiting", "revoked"];
    let before: Vec<_> = names.iter().map(|n| f.grant(n, "r1").unwrap()).collect();
    f.reload();
    let after: Vec<_> = names.iter().map(|n| f.grant(n, "r1").unwrap()).collect();
    assert_eq!(before, after);
    // The reload is not a new window: the waiting grant still ends at its fixed deadline.
    let deadline = T0 + 100 + READY_TO_RECONNECT_MS;
    assert_eq!(
        f.redeem("waiting", "r1", 2, deadline),
        Err(ContinuityRefusal::Expired)
    );
    for (name, refusal) in [
        ("redeemed", ContinuityRefusal::AlreadyRedeemed),
        ("revoked", ContinuityRefusal::Revoked),
    ] {
        let seed = if name == "redeemed" { 1 } else { 3 };
        assert_eq!(f.redeem(name, "r1", seed, deadline + 1), Err(refusal));
    }
}

#[test]
fn the_direct_database_path_sees_what_the_writer_wrote() {
    let mut f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.ready("r1", T0 + 10).unwrap();
    drop(f.storage.take());
    let database = Database::open(&f.config).unwrap();
    let view = database
        .cli_continuity_grant("inv-a", "r1")
        .unwrap()
        .unwrap();
    assert_eq!(view.state, GrantState::Ready);
    assert_eq!(
        database.cli_continuity_handoff_grants("r1").unwrap().grants,
        vec![view]
    );
    assert_eq!(
        database
            .redeem_cli_continuity(&binding("inv-a", "r1"), &token(7), T0 + 11)
            .unwrap(),
        Err(ContinuityRefusal::TokenMismatch)
    );
    let done = database
        .redeem_cli_continuity(&binding("inv-a", "r1"), &token(1), T0 + 12)
        .unwrap()
        .unwrap();
    assert_eq!(done.state, GrantState::Redeemed);
    assert_eq!(
        database
            .issue_cli_continuity(&binding("inv-a", "r1"), &token(1), T0 + 13)
            .unwrap(),
        Err(ContinuityRefusal::AlreadyIssued)
    );
}

#[test]
fn revocation_is_permanent_and_scoped() {
    let f = Fixture::new();
    f.restart("r2");
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.issue("inv-a", "r2", 2, T0).unwrap();
    f.issue("inv-b", "r1", 3, T0).unwrap();
    let handoff = f
        .handle()
        .revoke_cli_continuity(
            RevokeScope::Handoff {
                restart_id: "r1".into(),
            },
            T0 + 1,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        handoff.revoked,
        vec![binding("inv-a", "r1"), binding("inv-b", "r1")]
    );
    assert_eq!(f.state("inv-a", "r2"), GrantState::Issued);
    // Revoked before Ready: Ready does not wake it, and no token redeems it.
    assert!(f.ready("r1", T0 + 2).unwrap().ready.is_empty());
    assert_eq!(
        f.redeem("inv-a", "r1", 1, T0 + 3),
        Err(ContinuityRefusal::Revoked)
    );
    // Repeating is harmless and revokes nothing more.
    let again = f
        .handle()
        .revoke_cli_continuity(
            RevokeScope::Handoff {
                restart_id: "r1".into(),
            },
            T0 + 4,
        )
        .unwrap()
        .unwrap();
    assert!(again.revoked.is_empty());
    let one = f
        .handle()
        .revoke_cli_continuity(
            RevokeScope::Invocation {
                invocation_id: "inv-a".into(),
            },
            T0 + 5,
        )
        .unwrap()
        .unwrap();
    assert_eq!(one.revoked, vec![binding("inv-a", "r2")]);
    assert_eq!(
        f.handle()
            .revoke_cli_continuity(
                RevokeScope::Grant {
                    invocation_id: "nobody".into(),
                    restart_id: "r1".into()
                },
                T0 + 6
            )
            .unwrap(),
        Err(ContinuityRefusal::NotFound)
    );
    // A revoked Ready grant stays revoked too.
    f.restart("r4");
    f.issue("inv-c", "r4", 4, T0 + 7).unwrap();
    f.ready("r4", T0 + 8).unwrap();
    f.handle()
        .revoke_cli_continuity(
            RevokeScope::Handoff {
                restart_id: "r4".into(),
            },
            T0 + 9,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        f.redeem("inv-c", "r4", 4, T0 + 10),
        Err(ContinuityRefusal::Revoked)
    );
}

#[test]
fn expire_due_ends_only_the_grants_past_their_deadline() {
    let f = Fixture::new();
    f.restart("r2");
    f.issue("old", "r1", 1, T0).unwrap();
    f.issue("new", "r2", 2, T0 + 50_000).unwrap();
    let due = f
        .handle()
        .expire_due_cli_continuity(T0 + RESTART_TO_READY_MS)
        .unwrap()
        .unwrap();
    assert_eq!(due, vec![binding("old", "r1")]);
    assert_eq!(f.state("new", "r2"), GrantState::Issued);
    assert!(
        f.handle()
            .expire_due_cli_continuity(T0 + RESTART_TO_READY_MS)
            .unwrap()
            .unwrap()
            .is_empty()
    );
    let all = f
        .handle()
        .cli_continuity_handoff_grants("r1".into())
        .unwrap();
    assert_eq!((all.grants.len(), all.truncated), (1, false));
}

#[test]
fn live_grants_are_bounded_and_an_ended_one_frees_its_place() {
    let f = Fixture::new();
    for i in 0..MAX_LIVE_GRANTS {
        f.issue(&format!("inv-{i}"), "r1", 1, T0).unwrap();
    }
    assert_eq!(
        f.issue("one-more", "r1", 1, T0),
        Err(ContinuityRefusal::TooManyLive)
    );
    // Issuing after the window sweeps the overdue grants first, so the place is free.
    assert!(
        f.issue("one-more", "r1", 1, T0 + RESTART_TO_READY_MS)
            .is_ok()
    );
    assert_eq!(f.state("inv-0", "r1"), GrantState::Expired);
}

#[test]
fn malformed_input_is_refused_before_anything_is_written() {
    assert!(ContinuityToken::new(vec![1u8; TOKEN_MIN_BYTES - 1]).is_err());
    assert!(ContinuityToken::new(vec![1u8; TOKEN_MAX_BYTES + 1]).is_err());
    assert!(ContinuityToken::new(vec![1u8; TOKEN_MIN_BYTES]).is_ok());
    let f = Fixture::new();
    assert!(matches!(
        f.issue("", "r1", 1, T0),
        Err(ContinuityRefusal::Invalid(_))
    ));
    assert!(matches!(
        f.issue(&"x".repeat(MAX_ID_BYTES + 1), "r1", 1, T0),
        Err(ContinuityRefusal::Invalid(_))
    ));
    assert!(matches!(
        f.issue("line\nbreak", "r1", 1, T0),
        Err(ContinuityRefusal::Invalid(_))
    ));
    assert!(matches!(
        f.issue("inv-a", "r1", 1, MAX_TIME_MS + 1),
        Err(ContinuityRefusal::Invalid(_))
    ));
    assert!(f.grant("inv-a", "r1").is_none());
    // The accepted maximum leaves room for both deadlines.
    assert!(f.issue("inv-a", "r1", 1, MAX_TIME_MS).is_ok());
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn the_token_is_never_stored_or_shown() {
    let f = Fixture::new();
    let secret = ContinuityToken::new(MARKER).unwrap();
    assert!(!format!("{secret:?}").contains("PLAINTEXT"));
    f.handle()
        .issue_cli_continuity(binding("inv-a", "r1"), secret, T0)
        .unwrap()
        .unwrap();
    f.ready("r1", T0 + 1).unwrap();
    let again = ContinuityToken::new(MARKER).unwrap();
    let redeemed = f
        .handle()
        .redeem_cli_continuity(binding("inv-a", "r1"), again, T0 + 2)
        .unwrap()
        .unwrap();
    assert!(!format!("{redeemed:?}").contains("PLAINTEXT"));
    // Neither the database nor its log holds the token, and the digest column is not it.
    let sql = f.sql();
    let digest: Vec<u8> = sql
        .query_row("SELECT token_digest FROM cli_continuity_grants", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(digest.len(), 32);
    assert!(!contains(MARKER, &digest) && !contains(&digest, &MARKER[..32]));
    drop(sql);
    let scan = || {
        for suffix in ["", "-wal"] {
            let mut name = f.config.db_path.clone().into_os_string();
            name.push(suffix);
            if let Ok(bytes) = std::fs::read(&name) {
                assert!(!contains(&bytes, MARKER), "token found in {name:?}");
                assert!(
                    !contains(&bytes, &MARKER[..32]),
                    "token prefix found in {name:?}"
                );
            }
        }
    };
    scan();
    f.handle().checkpoint_now().unwrap();
    scan();
}

#[test]
fn undoing_v72_then_opening_migrates_again() {
    let mut f = Fixture::new();
    f.issue("inv-a", "r1", 1, T0).unwrap();
    drop(f.storage.take());
    let sql = f.sql();
    sql.execute_batch(concat!(
        include_str!("../undo/undo_v82.sql"),
        include_str!("../undo/undo_v81.sql"),
        include_str!("../undo/undo_v80.sql"),
        include_str!("../undo/undo_v79.sql"),
        include_str!("../undo/undo_v78.sql"),
        include_str!("../undo/undo_v77.sql"),
        include_str!("../undo/undo_v76.sql"),
        include_str!("../undo/undo_v75.sql"),
        include_str!("../undo/undo_v74.sql"),
        include_str!("../undo/undo_v73.sql"),
        include_str!("../undo/undo_v72.sql")
    ))
    .unwrap();
    sql.pragma_update(None, "user_version", 71).unwrap();
    drop(sql);
    let database = Database::open(&f.config).unwrap();
    assert!(
        database
            .cli_continuity_grant("inv-a", "r1")
            .unwrap()
            .is_none()
    );
    let version: u32 = f
        .sql()
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .unwrap();
    assert_eq!(version, 82);
    assert_eq!(
        database
            .issue_cli_continuity(&binding("inv-a", "r1"), &token(1), 1)
            .unwrap(),
        Ok(database
            .cli_continuity_grant("inv-a", "r1")
            .unwrap()
            .unwrap())
    );
}

#[test]
fn the_stored_digest_is_bound_to_the_invocation_and_the_handoff() {
    let f = Fixture::new();
    f.restart("r2");
    // One token, three grants: a copied digest proves nothing under another binding.
    f.issue("inv-a", "r1", 1, T0).unwrap();
    f.issue("inv-b", "r1", 1, T0).unwrap();
    f.issue("inv-a", "r2", 1, T0).unwrap();
    let digests: Vec<Vec<u8>> = f
        .sql()
        .prepare("SELECT token_digest FROM cli_continuity_grants")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let distinct: std::collections::BTreeSet<_> = digests.iter().collect();
    assert_eq!((digests.len(), distinct.len()), (3, 3));
}
