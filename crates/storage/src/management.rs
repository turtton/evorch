//! Conditional (compare-and-swap) updates of lessons and improvement candidates for the
//! management screens (ADR 0045, stage 14), and the one definition of what a revision
//! and an allowed action are.
//!
//! A client names the revision it saw. The single writer reads the entity, compares the
//! revision, judges the action against the current state and writes, all in one
//! `IMMEDIATE` transaction, so no other writer (learning, intake, another client) can
//! change the entity between the check and the write. The host must not read first and
//! write afterwards: that would leave exactly the window this module closes.
//!
//! * [`Database::memory_revision`] and [`improvement_revision`] are the revisions the
//!   read side shows; the update compares the same values.
//! * [`memory_denial`] and [`improvement_denial`] are the action rules the read side
//!   shows as `actions`; the update refuses exactly what they deny.
//! * A refusal is one of three distinct [`CasRefusal`]s, judged in this order: the
//!   entity does not exist, the revision is stale, the state forbids the action. A
//!   refused update writes nothing, so the revision does not move.
//!
//! The writes themselves reuse the existing mutations, so a conditional update and a
//! learning or intake write leave the same rows behind.

use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::improvement::{ImprovementCandidate, ImprovementStatus};
use crate::memory::MemoryStatus;
use crate::{Database, StorageError};

/// What a person can do to a lesson. The order of [`MemoryOp::ALL`] is the order the
/// read side lists the actions in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryOp {
    SetFavorite,
    ClearFavorite,
    Validate,
    Promote,
    Reject,
}

impl MemoryOp {
    pub const ALL: [Self; 5] = [
        Self::SetFavorite,
        Self::ClearFavorite,
        Self::Validate,
        Self::Promote,
        Self::Reject,
    ];
}

/// What a person can do to an improvement candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImprovementOp {
    MarkReviewed,
    Dismiss,
    /// Make a draft: its only write is [`ImprovementUpdate::AttachDraft`].
    Triage,
}

impl ImprovementOp {
    pub const ALL: [Self; 3] = [Self::MarkReviewed, Self::Dismiss, Self::Triage];
}

/// A conditional update of an improvement candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImprovementUpdate {
    MarkReviewed,
    Dismiss,
    /// Attach a draft reference, saved verbatim (the same rules as
    /// [`crate::StorageHandle::attach_improvement_draft`]); the action is
    /// [`ImprovementOp::Triage`].
    AttachDraft {
        draft_path: String,
    },
}

impl ImprovementUpdate {
    pub const fn op(&self) -> ImprovementOp {
        match self {
            Self::MarkReviewed => ImprovementOp::MarkReviewed,
            Self::Dismiss => ImprovementOp::Dismiss,
            Self::AttachDraft { .. } => ImprovementOp::Triage,
        }
    }
}

/// Why the state of an entity forbids an action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Denial {
    NotApplicableInState,
    /// A favorite lesson must stop being a favorite before it is rejected.
    FavoriteMustBeCleared,
    /// An earlier decision already settled the entity.
    AlreadyResolved,
}

/// Why a conditional update did nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasRefusal {
    /// The entity does not exist (never did, or was evicted).
    NotFound,
    /// The entity changed since the client read it; `current` is what it is now.
    StaleRevision { current: u64 },
    /// The current state forbids the action.
    Denied(Denial),
}

/// The result of a conditional update that reached the database. A storage failure is
/// the `Err` of the call instead and leaves the entity as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CasOutcome {
    /// Written; `revision` is the entity's revision after the write.
    Applied {
        revision: u64,
    },
    /// The action was allowed and the entity already is what it asks for, so nothing was
    /// written and the revision stays `revision`.
    Unchanged {
        revision: u64,
    },
    Refused(CasRefusal),
}

impl CasOutcome {
    pub const fn is_applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

/// Whether `op` may be done to a lesson in this state: the rule behind the lesson
/// actions the read side lists and behind [`crate::StorageHandle::update_memory`].
pub fn memory_denial(op: MemoryOp, status: MemoryStatus, favorite: bool) -> Option<Denial> {
    let rejected = status == MemoryStatus::Rejected;
    let settled = |ok: bool| {
        if ok {
            None
        } else if rejected {
            Some(Denial::AlreadyResolved)
        } else {
            Some(Denial::NotApplicableInState)
        }
    };
    match op {
        MemoryOp::SetFavorite => settled(!rejected && !favorite),
        MemoryOp::ClearFavorite => settled(!rejected && favorite),
        MemoryOp::Validate => settled(status == MemoryStatus::Candidate),
        MemoryOp::Promote => settled(status == MemoryStatus::Validated),
        MemoryOp::Reject if favorite && !rejected => Some(Denial::FavoriteMustBeCleared),
        MemoryOp::Reject => settled(!rejected),
    }
}

/// Whether `op` may be done to a candidate in this status (see [`memory_denial`]).
pub fn improvement_denial(op: ImprovementOp, status: ImprovementStatus) -> Option<Denial> {
    let already = |same: bool| same.then_some(Denial::AlreadyResolved);
    match op {
        ImprovementOp::MarkReviewed => already(status == ImprovementStatus::Reviewed),
        ImprovementOp::Dismiss => already(status == ImprovementStatus::Dismissed),
        ImprovementOp::Triage => {
            (status == ImprovementStatus::Dismissed).then_some(Denial::NotApplicableInState)
        }
    }
}

/// The revision of a candidate: a digest of the fields an update or a new occurrence
/// changes (status, occurrences, last seen, reopening, draft). Compare for equality only.
pub fn improvement_revision(candidate: &ImprovementCandidate) -> u64 {
    digest_revision(&(
        candidate.status.as_str(),
        candidate.occurrences,
        candidate.last_seen_at_ns,
        candidate.reopened_count,
        candidate.reopened_at_ns,
        &candidate.draft_path,
    ))
}

/// The first eight bytes of the SHA-256 of the value's JSON, big endian.
pub fn digest_revision<T: Serialize>(value: &T) -> u64 {
    let hash = Sha256::digest(serde_json::to_vec(value).expect("serializable view"));
    u64::from_be_bytes(hash[..8].try_into().expect("a digest is 32 bytes"))
}

impl Database {
    /// The revision of a lesson: its status rows plus its favorite changes. It grows on
    /// every change and only then (the ledger is append-only); a lesson that does not
    /// exist has revision 0.
    pub fn memory_revision(&self, id: &str) -> Result<u64, StorageError> {
        memory_revision_in(&self.conn, id)
    }
}

fn memory_revision_in(conn: &Connection, id: &str) -> Result<u64, StorageError> {
    let rows: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memory_ledger
         WHERE entry_id = ?1 AND kind IN ('lesson', 'favorite', 'unfavorite')",
        [id],
        |row| row.get(0),
    )?;
    u64::try_from(rows).map_err(|_| StorageError::OutOfRange("memory revision"))
}

fn immediate(conn: &Connection) -> Result<Transaction<'_>, StorageError> {
    Ok(Transaction::new_unchecked(
        conn,
        TransactionBehavior::Immediate,
    )?)
}

/// Runs on the writer. Dropping the transaction on any early return rolls it back.
pub(crate) fn update_memory(
    conn: &Connection,
    id: &str,
    expected: u64,
    op: MemoryOp,
) -> Result<CasOutcome, StorageError> {
    use crate::repo::memory::{Mutation, apply};

    let transaction = immediate(conn)?;
    let Some(entry) = crate::memory::entry_in(&transaction, id)? else {
        return Ok(CasOutcome::Refused(CasRefusal::NotFound));
    };
    let current = memory_revision_in(&transaction, id)?;
    if current != expected {
        return Ok(CasOutcome::Refused(CasRefusal::StaleRevision { current }));
    }
    if let Some(denial) = memory_denial(op, entry.status, entry.favorite) {
        return Ok(CasOutcome::Refused(CasRefusal::Denied(denial)));
    }
    let mutation = match op {
        MemoryOp::SetFavorite => Mutation::SetFavorite {
            id: id.into(),
            favorite: true,
        },
        MemoryOp::ClearFavorite => Mutation::SetFavorite {
            id: id.into(),
            favorite: false,
        },
        // The evidence is immutable per lesson id, so the lesson's own is what the
        // person validated.
        MemoryOp::Validate => Mutation::Validate {
            id: id.into(),
            evidence: entry.lesson.evidence.clone(),
        },
        MemoryOp::Promote => Mutation::Promote(id.into()),
        MemoryOp::Reject => Mutation::Reject(id.into()),
    };
    apply(&transaction, &mutation)?;
    let revision = memory_revision_in(&transaction, id)?;
    transaction.commit()?;
    Ok(CasOutcome::Applied { revision })
}

/// Runs on the writer; see [`update_memory`].
pub(crate) fn update_improvement(
    conn: &Connection,
    id: &str,
    expected: u64,
    update: &ImprovementUpdate,
) -> Result<CasOutcome, StorageError> {
    use crate::improvement::{attach_draft, candidate_in, set_status};

    let transaction = immediate(conn)?;
    let Some(candidate) = candidate_in(&transaction, id)? else {
        return Ok(CasOutcome::Refused(CasRefusal::NotFound));
    };
    let current = improvement_revision(&candidate);
    if current != expected {
        return Ok(CasOutcome::Refused(CasRefusal::StaleRevision { current }));
    }
    if let Some(denial) = improvement_denial(update.op(), candidate.status) {
        return Ok(CasOutcome::Refused(CasRefusal::Denied(denial)));
    }
    match update {
        ImprovementUpdate::MarkReviewed => {
            set_status(&transaction, id, ImprovementStatus::Reviewed)?;
        }
        ImprovementUpdate::Dismiss => {
            set_status(&transaction, id, ImprovementStatus::Dismissed)?;
        }
        ImprovementUpdate::AttachDraft { draft_path } => {
            if candidate.draft_path.as_deref() == Some(draft_path.as_str()) {
                return Ok(CasOutcome::Unchanged { revision: current });
            }
            attach_draft(&transaction, id, draft_path)?;
        }
    }
    let after = candidate_in(&transaction, id)?.ok_or_else(|| {
        StorageError::Serialization("improvement candidate vanished inside its update".into())
    })?;
    let revision = improvement_revision(&after);
    transaction.commit()?;
    Ok(CasOutcome::Applied { revision })
}
