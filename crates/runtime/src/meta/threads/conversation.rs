//! Phase B conversation search over the persisted index (ADR 0041 M5).
//!
//! Only runs of threads that already passed visibility enter a [`Stream`].
//! Storage narrows each scoped batch with a superset prefilter; every
//! candidate is then verified by the caller's matcher on re-redacted text, so
//! SQL routing never changes what counts as a match.

use std::collections::VecDeque;
use std::fmt::Write as _;

use storage::thread_index::{
    BlockLocator, GUARDED_REFERENCE_VERSION, GuardedReference, GuardedSource, IndexBlockKind,
};
use storage::thread_search::{
    CandidatePosition, ConversationCandidate, ConversationFilter, ConversationQuery,
    MAX_SEARCH_BATCH_RUNS,
};

use super::normalize::Ref;
use super::{Members, STORE_ERROR, Scope};

/// Candidate rows fetched per statement.
const PAGE_ROWS: usize = 200;

/// Runs whose conversation a thread contributes: the current root (whose
/// history contains the earlier turns) and, with `include_children`, every
/// delegated run. Ordered by run ID so a cursor can resume inside the thread.
pub(super) fn scan_runs(members: &Members, include_children: bool) -> Vec<String> {
    let mut runs: Vec<String> = members
        .default_scan()
        .into_iter()
        .filter(|run| include_children || members.primary.as_ref() == Some(run))
        .filter(|run| members.is_saved_member(run))
        .collect();
    runs.sort();
    runs.dedup();
    runs
}

/// Keyset-paged candidates over runs in scan order, at most
/// [`MAX_SEARCH_BATCH_RUNS`] runs per statement. A candidate's slot is the
/// index of its run in `runs`.
pub(super) struct Stream<'s> {
    scope: &'s Scope<'s>,
    runs: Vec<String>,
    kinds: Vec<IndexBlockKind>,
    filter: ConversationFilter,
    batch: usize,
    after: Option<CandidatePosition>,
    buffer: VecDeque<(usize, ConversationCandidate)>,
}

impl<'s> Stream<'s> {
    /// `first_block` starts the first run at that block ID (a resumed scan).
    pub(super) fn new(
        scope: &'s Scope<'s>,
        runs: Vec<String>,
        kinds: Vec<IndexBlockKind>,
        filter: ConversationFilter,
        first_block: Option<i64>,
    ) -> Self {
        Self {
            scope,
            runs,
            kinds,
            filter,
            batch: 0,
            after: first_block.map(|block| CandidatePosition {
                seq: 0,
                block_id: block.saturating_sub(1),
            }),
            buffer: VecDeque::new(),
        }
    }

    /// The next candidate without consuming it.
    pub(super) fn peek(&mut self) -> Result<Option<&(usize, ConversationCandidate)>, String> {
        while self.buffer.is_empty() && self.batch < self.runs.len() {
            self.fetch()?;
        }
        Ok(self.buffer.front())
    }

    pub(super) fn pop(&mut self) -> Option<(usize, ConversationCandidate)> {
        self.buffer.pop_front()
    }

    fn fetch(&mut self) -> Result<(), String> {
        let end = (self.batch + MAX_SEARCH_BATCH_RUNS).min(self.runs.len());
        let runs = &self.runs[self.batch..end];
        let rows = self
            .scope
            .store
            .with_database(|db| {
                db.conversation_candidates(&ConversationQuery {
                    runs,
                    kinds: &self.kinds,
                    filter: &self.filter,
                    after: self.after,
                    limit: PAGE_ROWS,
                })
            })
            .map_err(|_| STORE_ERROR.to_owned())?;
        let batch = self.batch;
        let full = rows.len() == PAGE_ROWS;
        self.after = rows.last().map(|row| row.position).filter(|_| full);
        if !full {
            self.batch = end;
        }
        self.buffer
            .extend(rows.into_iter().map(|row| (batch + row.position.seq, row)));
        Ok(())
    }
}

/// `run-…#<ordinal>.<block>@v1.s<snapshot>.r<source>` for a message block, or
/// `run-…#note.<seq>@v1.r<source>` for a working note. The part before `@` of
/// a message ref is the unguarded ref, which keeps working on its own.
pub(super) fn guarded_text(reference: &GuardedReference) -> String {
    let mut text = reference.run_id.clone();
    match reference.source {
        GuardedSource::Message {
            snapshot_revision,
            ordinal,
            block_index,
        } => {
            let _ = write!(
                text,
                "#{ordinal}.{block_index}@v{}.s{snapshot_revision}.r{}",
                reference.version, reference.source_revision
            );
        }
        GuardedSource::WorkingNote { note_seq } => {
            let _ = write!(
                text,
                "#note.{note_seq}@v{}.r{}",
                reference.version, reference.source_revision
            );
        }
    }
    text
}

pub(super) fn guarded(candidate: &ConversationCandidate) -> GuardedReference {
    GuardedReference {
        version: GUARDED_REFERENCE_VERSION,
        run_id: candidate.run_id.clone(),
        source_revision: candidate.source_revision,
        source: match candidate.locator {
            BlockLocator::Message {
                ordinal,
                block_index,
            } => GuardedSource::Message {
                snapshot_revision: candidate.snapshot_revision,
                ordinal,
                block_index,
            },
            BlockLocator::WorkingNote { note_seq } => GuardedSource::WorkingNote { note_seq },
        },
    }
}

/// An `around` argument: an unguarded message ref, or a revision-checked one.
pub(super) enum Around {
    Plain(Ref),
    Guarded(GuardedReference),
}

impl Around {
    pub(super) fn run_id(&self) -> String {
        match self {
            Self::Plain(reference) => reference.run_id.to_string(),
            Self::Guarded(reference) => reference.run_id.clone(),
        }
    }
}

pub(super) fn parse_around(text: &str) -> Result<Around, String> {
    let Some((locator, guard)) = text.rsplit_once('@') else {
        return text.parse().map(Around::Plain);
    };
    let invalid = || {
        "invalid thread ref; expected run-…#<ordinal>.<block>, optionally with the @v… guard thread_search returns"
            .to_owned()
    };
    let digits = |text: &str| -> Result<u64, String> {
        if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err(invalid());
        }
        text.parse().map_err(|_| invalid())
    };
    let tagged = |part: Option<&str>, tag: char| {
        digits(
            part.and_then(|part| part.strip_prefix(tag))
                .ok_or_else(invalid)?,
        )
    };
    let mut parts = guard.split('.');
    let version = u32::try_from(tagged(parts.next(), 'v')?).map_err(|_| invalid())?;
    let reference = if let Some((run, seq)) = locator.split_once("#note.") {
        let run_id: crate::RunId = run.parse().map_err(|_| invalid())?;
        GuardedReference {
            version,
            run_id: run_id.to_string(),
            source_revision: tagged(parts.next(), 'r')?,
            source: GuardedSource::WorkingNote {
                note_seq: digits(seq)?,
            },
        }
    } else {
        let plain: Ref = locator.parse().map_err(|_| invalid())?;
        let snapshot_revision = tagged(parts.next(), 's')?;
        GuardedReference {
            version,
            run_id: plain.run_id.to_string(),
            source_revision: tagged(parts.next(), 'r')?,
            source: GuardedSource::Message {
                snapshot_revision,
                ordinal: plain.ordinal,
                block_index: plain.block,
            },
        }
    };
    if parts.next().is_some() {
        return Err(invalid());
    }
    Ok(Around::Guarded(reference))
}
