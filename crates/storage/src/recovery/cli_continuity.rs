//! CLI continuity storage (5b-D): the durable record that lets one CLI invocation
//! continue across one planned restart.
//!
//! A grant is evidence for a later check, never an authority. It binds an invocation to
//! a handoff (the durable restart transaction and its capture), holds the digest of a
//! token (never the token itself) and carries two deadlines that this module computes
//! from constants: [`RESTART_TO_READY_MS`] from issue to Ready and
//! [`READY_TO_RECONNECT_MS`] from Ready to reconnect. No call accepts a deadline, and
//! none can set, extend or restart one: the schema pins the arithmetic and the direction
//! (see `migrations/v72.sql`), so a raw `UPDATE` cannot either.
//!
//! The module has no issuer and no admission. Whoever hands out the token (the fixture
//! issuer of 5b-R, the production issuer of 17) generates it and passes it here once at
//! [`Database::issue_cli_continuity`]; a later [`Database::redeem_cli_continuity`] proves
//! possession and consumes the grant exactly once. Success is a fact for the host and
//! the runtime to check again with their own gates, not a permission.
//!
//! Time comes from the caller in milliseconds and must come from one clock for every
//! call. The writer keeps the highest time it was ever given. A smaller time is refused
//! as [`ContinuityRefusal::ClockRegressed`], and because a clock that moved back would
//! otherwise let a grant outlive its fixed deadline once it caught up again, the refusal
//! also ends every live grant as `expired`, for good. An overdue grant is ended as
//! `expired` the next time any call observes it, and an ended grant never changes again.
use super::*;
use crate::writer::StorageHandle;
use rusqlite::{Transaction, TransactionBehavior};
use sha2::{Digest, Sha256};

mod store;

/// Fixed time from the issue of a grant until the successor must be Ready. A grant that
/// is not Ready by then is expired; a late Ready cannot revive it.
pub const RESTART_TO_READY_MS: u64 = 120_000;
/// Fixed time from Ready until the invocation must have reconnected and redeemed.
pub const READY_TO_RECONNECT_MS: u64 = 30_000;
/// Most grants that are neither redeemed, revoked nor expired.
pub const MAX_LIVE_GRANTS: usize = 256;
/// Longest identifier, in bytes.
pub const MAX_ID_BYTES: usize = 128;
/// Shortest and longest token, in bytes. The floor rejects a guessable token.
pub const TOKEN_MIN_BYTES: usize = 32;
pub const TOKEN_MAX_BYTES: usize = 128;
/// Largest time the module accepts, so that no deadline overflows a SQLite integer.
pub const MAX_TIME_MS: u64 = (i64::MAX as u64) - RESTART_TO_READY_MS - READY_TO_RECONNECT_MS;

const DIGEST_DOMAIN: &[u8] = b"evorch/cli-continuity/v1";

/// A secret that proves an invocation may continue. It is never copied, serialized or
/// shown: `Debug` prints a placeholder, and the module stores only its digest.
pub struct ContinuityToken(Vec<u8>);
impl ContinuityToken {
    /// # Errors
    /// [`ContinuityRefusal::Invalid`] when the length is outside
    /// [`TOKEN_MIN_BYTES`]`..=`[`TOKEN_MAX_BYTES`].
    pub fn new(bytes: impl Into<Vec<u8>>) -> Result<Self, ContinuityRefusal> {
        let bytes = bytes.into();
        if !(TOKEN_MIN_BYTES..=TOKEN_MAX_BYTES).contains(&bytes.len()) {
            return Err(ContinuityRefusal::Invalid("token length"));
        }
        Ok(Self(bytes))
    }
    /// The digest, bound to the grant it protects: a digest taken for another invocation,
    /// restart or capture differs, so a copied digest proves nothing elsewhere.
    fn digest(&self, binding: &ContinuityBinding) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(DIGEST_DOMAIN);
        for part in [
            binding.invocation_id.as_bytes(),
            binding.restart_id.as_bytes(),
            binding.capture_id.as_bytes(),
            &self.0,
        ] {
            hash.update((part.len() as u32).to_be_bytes());
            hash.update(part);
        }
        hash.finalize().into()
    }
}
impl std::fmt::Debug for ContinuityToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContinuityToken(..)")
    }
}

/// The invocation and the handoff a grant is bound to. The handoff is the durable
/// restart transaction: `restart_id` with the `capture_id` it was captured under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContinuityBinding {
    pub invocation_id: String,
    pub restart_id: String,
    pub capture_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantState {
    /// Waiting for the successor to become Ready.
    Issued,
    /// Ready; waiting for the invocation to reconnect.
    Ready,
    /// The invocation proved possession. Final.
    Redeemed,
    /// Withdrawn. Final.
    Revoked,
    /// A deadline passed first. Final.
    Expired,
}
impl GrantState {
    pub fn live(self) -> bool {
        matches!(self, Self::Issued | Self::Ready)
    }
}
/// What is stored about a grant. The digest is never part of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrantView {
    pub binding: ContinuityBinding,
    pub state: GrantState,
    pub issued_at_ms: u64,
    /// `issued_at_ms` + [`RESTART_TO_READY_MS`]; the grant expires at this time.
    pub ready_deadline_ms: u64,
    pub ready_at_ms: Option<u64>,
    /// `ready_at_ms` + [`READY_TO_RECONNECT_MS`]; the grant expires at this time.
    pub reconnect_deadline_ms: Option<u64>,
    /// When the grant was redeemed, revoked, or observed expired.
    pub ended_at_ms: Option<u64>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffGrants {
    pub grants: Vec<GrantView>,
    /// More grants exist than [`MAX_LIVE_GRANTS`].
    pub truncated: bool,
}
/// Result of [`Database::mark_cli_handoff_ready`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandoffReady {
    /// Grants that became Ready by this call, each with its own fixed reconnect deadline.
    pub ready: Vec<ContinuityBinding>,
    /// Grants of this handoff that this call ended as expired. They stay ended.
    pub expired: Vec<ContinuityBinding>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RevokeScope {
    /// One grant. A redeemed grant is refused: revocation does not undo a redemption.
    Grant {
        invocation_id: String,
        restart_id: String,
    },
    /// Every live grant of an invocation (it was cancelled or disconnected).
    Invocation { invocation_id: String },
    /// Every live grant of a handoff (it was aborted or invalidated).
    Handoff { restart_id: String },
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RevokeReport {
    /// Grants this call revoked. Empty when everything in scope had already ended.
    pub revoked: Vec<ContinuityBinding>,
}

/// A definite refusal. Nothing was granted, and no deadline moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContinuityRefusal {
    /// An identifier, token or time is outside its bounds.
    Invalid(&'static str),
    /// The time is below one the writer has already seen. Every live grant was ended as
    /// expired by this refusal; nothing else changed.
    ClockRegressed,
    /// No such restart transaction.
    HandoffNotFound,
    /// The capture is not the one the restart transaction was made under.
    HandoffMismatch,
    /// The restart transaction already ended.
    HandoffEnded,
    /// This invocation already has a grant for this handoff, whatever became of it.
    /// Read it back; a second issue never restarts a deadline.
    AlreadyIssued,
    /// [`MAX_LIVE_GRANTS`] grants are live.
    TooManyLive,
    /// This invocation has no grant for this handoff.
    NotFound,
    /// The token is not the one the grant was issued with.
    TokenMismatch,
    /// The successor is not Ready yet.
    NotReady,
    Expired,
    Revoked,
    AlreadyRedeemed,
}
pub type ContinuityReport<T> = Result<T, ContinuityRefusal>;

fn invalid(reason: &'static str) -> ContinuityRefusal {
    ContinuityRefusal::Invalid(reason)
}
fn check_id(value: &str, what: &'static str) -> ContinuityReport<()> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        return Err(invalid(what));
    }
    Ok(())
}
fn check_binding(binding: &ContinuityBinding) -> ContinuityReport<()> {
    check_id(&binding.invocation_id, "invocation_id")?;
    check_id(&binding.restart_id, "restart_id")?;
    check_id(&binding.capture_id, "capture_id")
}
fn check_time(now_ms: u64) -> ContinuityReport<()> {
    if now_ms > MAX_TIME_MS {
        return Err(invalid("time"));
    }
    Ok(())
}

/// One FULL-durable transaction, committed whatever the report says: a refusal may still
/// have ended an overdue grant, and that is a fact that must stay.
fn write<T>(
    conn: &Connection,
    work: impl FnOnce(&Transaction<'_>) -> Result<T, StorageError>,
) -> Result<T, StorageError> {
    let previous: i64 = conn.pragma_query_value(None, "synchronous", |r| r.get(0))?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    let result = (|| {
        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
        let value = work(&tx)?;
        tx.commit()?;
        Ok(value)
    })();
    // Never turn a committed transaction into a retryable error.
    if let Err(error) = conn.pragma_update(None, "synchronous", previous) {
        tracing::warn!(%error, "cli continuity retained FULL synchronous mode");
    }
    result
}

impl Database {
    /// Records a grant for `binding` and starts its [`RESTART_TO_READY_MS`] window now.
    /// Call it when the Restarting phase begins.
    ///
    /// # Errors
    /// SQLite failures. A refusal is the inner result.
    pub fn issue_cli_continuity(
        &self,
        binding: &ContinuityBinding,
        token: &ContinuityToken,
        now_ms: u64,
    ) -> Result<ContinuityReport<GrantView>, StorageError> {
        write(&self.conn, |tx| store::issue(tx, binding, token, now_ms))
    }
    /// The successor of `restart_id` is Ready: each grant still waiting gets its fixed
    /// reconnect deadline, counted from `now_ms`. Calling again moves nothing.
    ///
    /// # Errors
    /// SQLite failures. A refusal is the inner result.
    pub fn mark_cli_handoff_ready(
        &self,
        restart_id: &str,
        capture_id: &str,
        now_ms: u64,
    ) -> Result<ContinuityReport<HandoffReady>, StorageError> {
        write(&self.conn, |tx| {
            store::mark_ready(tx, restart_id, capture_id, now_ms)
        })
    }
    /// Checks the token against the grant and, only when everything holds, consumes the
    /// grant. Once, never again.
    ///
    /// # Errors
    /// SQLite failures. A refusal is the inner result.
    pub fn redeem_cli_continuity(
        &self,
        binding: &ContinuityBinding,
        token: &ContinuityToken,
        now_ms: u64,
    ) -> Result<ContinuityReport<GrantView>, StorageError> {
        write(&self.conn, |tx| store::redeem(tx, binding, token, now_ms))
    }
    /// Permanently withdraws live grants.
    ///
    /// # Errors
    /// SQLite failures. A refusal is the inner result.
    pub fn revoke_cli_continuity(
        &self,
        scope: &RevokeScope,
        now_ms: u64,
    ) -> Result<ContinuityReport<RevokeReport>, StorageError> {
        write(&self.conn, |tx| store::revoke(tx, scope, now_ms))
    }
    /// Ends every grant whose deadline has passed and says which.
    ///
    /// # Errors
    /// SQLite failures. A refusal is the inner result.
    pub fn expire_due_cli_continuity(
        &self,
        now_ms: u64,
    ) -> Result<ContinuityReport<Vec<ContinuityBinding>>, StorageError> {
        write(&self.conn, |tx| store::expire_due(tx, now_ms))
    }
    /// One grant as stored. A grant past its deadline reads as it was last written until a
    /// call that observes time ends it.
    ///
    /// # Errors
    /// SQLite failures or an unreadable row.
    pub fn cli_continuity_grant(
        &self,
        invocation_id: &str,
        restart_id: &str,
    ) -> Result<Option<GrantView>, StorageError> {
        store::grant(&self.conn, invocation_id, restart_id)
    }
    /// The grants of one handoff, oldest first.
    ///
    /// # Errors
    /// SQLite failures or an unreadable row.
    pub fn cli_continuity_handoff_grants(
        &self,
        restart_id: &str,
    ) -> Result<HandoffGrants, StorageError> {
        store::handoff_grants(&self.conn, restart_id)
    }
}

impl StorageHandle {
    /// See [`Database::issue_cli_continuity`]. The token is consumed: it does not outlive
    /// the call. An outer error means the answer is unknown: read the grant back.
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn issue_cli_continuity(
        &self,
        binding: ContinuityBinding,
        token: ContinuityToken,
        now_ms: u64,
    ) -> Result<ContinuityReport<GrantView>, StorageError> {
        self.lane_job(true, move |conn| {
            write(conn, |tx| store::issue(tx, &binding, &token, now_ms))
        })
    }
    /// See [`Database::mark_cli_handoff_ready`].
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn mark_cli_handoff_ready(
        &self,
        restart_id: String,
        capture_id: String,
        now_ms: u64,
    ) -> Result<ContinuityReport<HandoffReady>, StorageError> {
        self.lane_job(true, move |conn| {
            write(conn, |tx| {
                store::mark_ready(tx, &restart_id, &capture_id, now_ms)
            })
        })
    }
    /// See [`Database::redeem_cli_continuity`]. After an outer error, read the grant back:
    /// do not assume it is still unredeemed.
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn redeem_cli_continuity(
        &self,
        binding: ContinuityBinding,
        token: ContinuityToken,
        now_ms: u64,
    ) -> Result<ContinuityReport<GrantView>, StorageError> {
        self.lane_job(true, move |conn| {
            write(conn, |tx| store::redeem(tx, &binding, &token, now_ms))
        })
    }
    /// See [`Database::revoke_cli_continuity`].
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn revoke_cli_continuity(
        &self,
        scope: RevokeScope,
        now_ms: u64,
    ) -> Result<ContinuityReport<RevokeReport>, StorageError> {
        self.lane_job(true, move |conn| {
            write(conn, |tx| store::revoke(tx, &scope, now_ms))
        })
    }
    /// See [`Database::expire_due_cli_continuity`].
    ///
    /// # Errors
    /// A closed writer, suspended writes or a SQLite failure.
    pub fn expire_due_cli_continuity(
        &self,
        now_ms: u64,
    ) -> Result<ContinuityReport<Vec<ContinuityBinding>>, StorageError> {
        self.lane_job(true, move |conn| {
            write(conn, |tx| store::expire_due(tx, now_ms))
        })
    }
    /// See [`Database::cli_continuity_grant`].
    ///
    /// # Errors
    /// A closed writer or a SQLite failure.
    pub fn cli_continuity_grant(
        &self,
        invocation_id: String,
        restart_id: String,
    ) -> Result<Option<GrantView>, StorageError> {
        self.lane_job(false, move |conn| {
            store::grant(conn, &invocation_id, &restart_id)
        })
    }
    /// See [`Database::cli_continuity_handoff_grants`].
    ///
    /// # Errors
    /// A closed writer or a SQLite failure.
    pub fn cli_continuity_handoff_grants(
        &self,
        restart_id: String,
    ) -> Result<HandoffGrants, StorageError> {
        self.lane_job(false, move |conn| store::handoff_grants(conn, &restart_id))
    }
}
