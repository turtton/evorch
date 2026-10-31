//! ADR 0041 Phase B conversation search reads (order 6).
//!
//! The caller authorizes first and passes only runs it may read, at most
//! [`MAX_SEARCH_BATCH_RUNS`] per query. That is a per-query batch bound, not a
//! total bound: callers page through any number of runs batch by batch. Each
//! ordered stream checks its current indexed run before visiting blocks or FTS,
//! so rows, order and keyset positions never depend on
//! other runs, nor does the number of block candidates evaluated.
//!
//! Text filters are a *superset* prefilter. Callers must verify each candidate
//! with their own matcher; [`term_route`] only promises that no block a
//! case-insensitive substring match would accept is filtered out. Blocks are
//! returned only while their run is `indexed` at its current `source_revision`,
//! so a stale index never contributes a hit.
//!
//! [`Database::conversation_scan_chunk`] is the filter-free alternative: it
//! reads the same ordered stream lazily, stops at a block or byte bound and
//! finalizes its statement before returning, so a caller can verify blocks
//! with its own matcher without FTS work or a long-lived read snapshot.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Row, params_from_iter};

use crate::thread_index::{BlockLocator, EXTRACTOR_VERSION, IndexBlockKind, IndexRole};
use crate::{Database, StorageError};

mod tokenizer_prefilter;

#[cfg(test)]
mod tests;

/// Largest number of runs one statement is scoped to.
pub const MAX_SEARCH_BATCH_RUNS: usize = 100;

/// Largest number of candidate rows one statement returns.
pub const MAX_SEARCH_PAGE_ROWS: usize = 500;

/// Case-variant spellings one term may expand to before it falls back to a
/// scan; see [`term_route`].
pub const MAX_TERM_VARIANTS: usize = 16;

/// Shared by coverage, every candidate route and therefore runtime ownership
/// and cursor validation. Deferred settings must not expose old extraction.
fn current_index_sql() -> String {
    format!(
        "r.state = 'indexed' AND r.source_present = 1 \
         AND r.indexed_revision = r.source_revision \
         AND r.extractor_version = {EXTRACTOR_VERSION} \
         AND r.indexed_extractor_version = {EXTRACTOR_VERSION}"
    )
}

/// How one case-insensitive literal term can be narrowed in SQL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermRoute {
    /// At least 3 scalars and fold-safe: FTS5 trigram phrases, OR'ed.
    Trigram(Vec<String>),
    /// Fewer than 3 scalars and fold-safe: escaped `LIKE` substrings, OR'ed.
    Like(Vec<String>),
    /// No SQL narrowing is guaranteed to keep every match; scan and verify.
    Scan,
}

/// Route a case-insensitive literal term.
///
/// FTS5 trigram (`case_sensitive=0`) and SQLite `LIKE` both fold ASCII only
/// reliably, while Unicode simple case folding (the caller's matcher) also
/// pairs ASCII `k`/`s` with U+212A KELVIN SIGN and U+017F LATIN SMALL LETTER
/// LONG S. Those two are spelled out as extra variants. Any other cased
/// non-ASCII scalar has no SQL guarantee, so the term is scanned. Trigram
/// cannot match fewer than 3 scalars, so short terms use `LIKE`.
#[must_use]
pub fn term_route(term: &str) -> TermRoute {
    let mut variants = vec![String::new()];
    for c in term.chars() {
        let spellings: &[char] = match c {
            'k' | 'K' => &['k', '\u{212A}'],
            's' | 'S' => &['s', '\u{17F}'],
            '\0' => return TermRoute::Scan,
            c if c.is_ascii() => &[c],
            c if is_cased(c) => return TermRoute::Scan,
            _ => &[c],
        };
        if variants.len() * spellings.len() > MAX_TERM_VARIANTS {
            return TermRoute::Scan;
        }
        variants = variants
            .iter()
            .flat_map(|prefix| {
                spellings.iter().map(move |spelling| {
                    let mut next = prefix.clone();
                    next.push(*spelling);
                    next
                })
            })
            .collect();
    }
    if term.chars().count() >= 3 {
        TermRoute::Trigram(variants)
    } else {
        TermRoute::Like(variants)
    }
}

/// A scalar with any case mapping can have case-variant spellings that the
/// SQL folds do not pair with it.
fn is_cased(c: char) -> bool {
    !c.to_lowercase().eq([c]) || !c.to_uppercase().eq([c])
}

/// Which blocks a query may return before the caller verifies them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConversationFilter {
    /// Every block of the requested kinds: regular expressions, and literal
    /// queries without a SQL guarantee.
    All,
    /// OR of alternatives; an alternative needs every one of its
    /// case-insensitive literal terms in the same block.
    Literals(Vec<Vec<String>>),
}

/// Keyset position inside one batch: after `block_id` of the run at `seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CandidatePosition {
    pub seq: usize,
    pub block_id: i64,
}

/// One scoped, keyset-paged candidate query.
#[derive(Debug, Clone, Copy)]
pub struct ConversationQuery<'a> {
    /// Authorized runs in the caller's scan order; `seq` is the index here.
    pub runs: &'a [String],
    pub kinds: &'a [IndexBlockKind],
    pub filter: &'a ConversationFilter,
    /// Resume strictly after this position.
    pub after: Option<CandidatePosition>,
    /// 1..=[`MAX_SEARCH_PAGE_ROWS`].
    pub limit: usize,
}

/// One unfiltered, keyset-paged scan of the blocks of `runs`.
///
/// Unlike [`ConversationQuery`] there is no text filter: the caller verifies
/// every returned block with its own matcher, so the work of one call is
/// bounded by [`ScanLimits`] and never by how rare a match is.
#[derive(Debug, Clone, Copy)]
pub struct ConversationScan<'a> {
    /// Authorized runs in the caller's scan order; `seq` is the index here.
    pub runs: &'a [String],
    pub kinds: &'a [IndexBlockKind],
    /// Resume strictly after this position.
    pub after: Option<CandidatePosition>,
}

/// Work bounds of one [`Database::conversation_scan_chunk`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanLimits {
    /// 1..=[`MAX_SEARCH_PAGE_ROWS`] blocks.
    pub max_blocks: usize,
    /// At least 1. Scanning stops after the first block that brings the
    /// accumulated text bytes to this value, so one chunk may exceed it by
    /// at most one block.
    pub max_bytes: usize,
}

/// A bounded run of blocks in `(seq, block id)` order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanChunk {
    pub blocks: Vec<ConversationCandidate>,
    /// Sum of `text.len()` over `blocks`.
    pub scanned_bytes: usize,
    /// The statement had no further row. Stopping at a limit leaves this
    /// `false`, even if nothing remains; the next call then returns no block
    /// and `true`.
    pub exhausted: bool,
}

/// A block of an indexed run at its current revision. `text` is the
/// extracted, redacted and truncated index text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationCandidate {
    pub position: CandidatePosition,
    pub run_id: String,
    pub source_revision: u64,
    pub snapshot_revision: u64,
    pub locator: BlockLocator,
    pub role: IndexRole,
    pub kind: IndexBlockKind,
    pub tool_name: Option<String>,
    pub truncated: bool,
    pub text: String,
}

/// Index coverage of one run, from the caller's point of view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunCoverage {
    /// Searchable at its current source revision.
    Indexed,
    /// Waiting for (re)extraction, including an `indexed` row whose blocks
    /// belong to an older revision.
    Pending,
    RetryWait,
    Blocked,
    Evicted,
    /// The source was deleted (tombstone).
    Deleted,
    /// Discovery has not reached the run yet.
    Undiscovered,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIndexCoverage {
    pub run_id: String,
    pub coverage: RunCoverage,
    /// Current source revision, if the run has an index row.
    pub source_revision: Option<u64>,
}

/// Aggregated coverage. `pending` includes retry_wait and blocked; `missing`
/// covers undiscovered and deleted sources.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexCoverageSummary {
    pub indexed: u64,
    pub pending: u64,
    pub evicted: u64,
    pub missing: u64,
    /// Runs the index job will still reach without an explicit request:
    /// pending, retry_wait and undiscovered.
    pub waiting: u64,
}

impl IndexCoverageSummary {
    pub fn add(&mut self, coverage: RunCoverage) {
        match coverage {
            RunCoverage::Indexed => self.indexed += 1,
            RunCoverage::Pending | RunCoverage::RetryWait => {
                self.pending += 1;
                self.waiting += 1;
            }
            RunCoverage::Blocked => self.pending += 1,
            RunCoverage::Evicted => self.evicted += 1,
            RunCoverage::Deleted => self.missing += 1,
            RunCoverage::Undiscovered => {
                self.missing += 1;
                self.waiting += 1;
            }
        }
    }

    #[must_use]
    pub const fn unindexed(&self) -> u64 {
        self.pending + self.evicted + self.missing
    }

    #[must_use]
    pub const fn backfill_in_progress(&self) -> bool {
        self.waiting > 0
    }
}

impl Database {
    /// Candidate blocks of `query.runs`, ordered by `(seq, block id)`.
    ///
    /// # Errors
    /// Returns an error for an empty or oversized batch, an invalid limit, or
    /// SQLite failure.
    pub fn conversation_candidates(
        &self,
        query: &ConversationQuery<'_>,
    ) -> Result<Vec<ConversationCandidate>, StorageError> {
        check_batch(query.runs)?;
        if !(1..=MAX_SEARCH_PAGE_ROWS).contains(&query.limit) || query.kinds.is_empty() {
            return Err(StorageError::Serialization(
                "conversation search needs 1-500 rows and at least one kind".into(),
            ));
        }
        let (sql, params) = conversation_candidate_sql(query, &self.conn);
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), candidate)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The next bounded chunk of blocks of `scan.runs`, ordered by
    /// `(seq, block id)` and strictly after `scan.after`.
    ///
    /// Only blocks of runs that are `indexed` at their current revision are
    /// returned. The statement is prepared, stepped just far enough to reach
    /// a limit, and finalized before returning, so no read snapshot outlives
    /// the call (a live one would block WAL checkpoints).
    ///
    /// # Errors
    /// Returns an error for an empty or oversized batch, no kinds, invalid
    /// limits, or SQLite failure.
    pub fn conversation_scan_chunk(
        &self,
        scan: &ConversationScan<'_>,
        limits: ScanLimits,
    ) -> Result<ScanChunk, StorageError> {
        self.scan_chunk_observed(scan, limits, &mut |_| {})
    }

    /// [`Self::conversation_scan_chunk`] that lets the work-counter tests read
    /// the statement just before it is finalized.
    fn scan_chunk_observed(
        &self,
        scan: &ConversationScan<'_>,
        limits: ScanLimits,
        observe: &mut dyn FnMut(&rusqlite::Statement<'_>),
    ) -> Result<ScanChunk, StorageError> {
        check_batch(scan.runs)?;
        if !(1..=MAX_SEARCH_PAGE_ROWS).contains(&limits.max_blocks)
            || limits.max_bytes == 0
            || scan.kinds.is_empty()
        {
            return Err(StorageError::Serialization(
                "conversation scan needs 1-500 blocks, 1+ bytes and at least one kind".into(),
            ));
        }
        // SQL LIMIT = max_blocks only stops the statement where the loop
        // below stops anyway; it never hides a row the loop would read.
        let (sql, params) = conversation_scan_sql(scan, limits.max_blocks);
        let mut chunk = ScanChunk {
            blocks: Vec::new(),
            scanned_bytes: 0,
            exhausted: false,
        };
        let mut statement = self.conn.prepare(&sql)?;
        let mut rows = statement.query(params_from_iter(params))?;
        loop {
            let Some(row) = rows.next()? else {
                chunk.exhausted = true;
                break;
            };
            let block = candidate(row)?;
            chunk.scanned_bytes = chunk.scanned_bytes.saturating_add(block.text.len());
            chunk.blocks.push(block);
            if chunk.blocks.len() >= limits.max_blocks || chunk.scanned_bytes >= limits.max_bytes {
                break;
            }
        }
        drop(rows);
        observe(&statement);
        drop(statement);
        Ok(chunk)
    }

    /// Coverage of each given run, in input order.
    ///
    /// # Errors
    /// Returns an error for an empty or oversized batch or SQLite failure.
    pub fn thread_index_coverage(
        &self,
        runs: &[String],
    ) -> Result<Vec<RunIndexCoverage>, StorageError> {
        check_batch(runs)?;
        let mut params = Vec::new();
        let scope = scope_values(runs, &mut params);
        let sql = format!(
            "WITH scope(run_id, seq) AS (VALUES {scope}) \
             SELECT scope.run_id, r.state, r.source_present, r.source_revision, \
                    ({current}) \
             FROM scope LEFT JOIN thread_index_runs r ON r.run_id = scope.run_id \
             ORDER BY scope.seq",
            current = current_index_sql(),
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), |row| {
            let state: Option<String> = row.get(1)?;
            let present: Option<bool> = row.get(2)?;
            let source: Option<u64> = row.get(3)?;
            let current: Option<bool> = row.get(4)?;
            let coverage = match (state.as_deref(), present) {
                (None, _) => RunCoverage::Undiscovered,
                (Some("deleted"), _) | (_, Some(false)) => RunCoverage::Deleted,
                (Some("evicted"), _) => RunCoverage::Evicted,
                (Some("blocked"), _) => RunCoverage::Blocked,
                (Some("retry_wait"), _) => RunCoverage::RetryWait,
                (Some("indexed"), _) if current == Some(true) => RunCoverage::Indexed,
                _ => RunCoverage::Pending,
            };
            Ok(RunIndexCoverage {
                run_id: row.get(0)?,
                coverage,
                source_revision: source,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}

/// Keep the production statement shared with the plan/work regression tests.
fn conversation_candidate_sql(
    query: &ConversationQuery<'_>,
    conn: &rusqlite::Connection,
) -> (String, Vec<SqlValue>) {
    // Each arm has one run and one kind fixed. The existing (kind, run_id)
    // index therefore emits rowids in order, with no candidate-wide sort.
    // SQLite's ordered UNION ALL merges these streams lazily under the one
    // global LIMIT. The predicate is evaluated BEFORE that limit: limiting
    // unfiltered blocks would skip later matches and change the cursor.
    //
    // Keep one statement, hence one SQLite read snapshot. Duplicate run IDs
    // deliberately produce separate arms/seqs; duplicate kinds do not.
    let kinds = unique_kinds(query.kinds);
    let mut params = Vec::new();
    let exact = tokenizer_prefilter::install(conn);
    let filter = filter_sql(query.filter, &mut params, exact);
    // Reuse the same predicate bindings in every arm (including auxdata's
    // constant QUERY input), rather than multiplying SQLite variable usage.
    let mut parameter = 0;
    let filter = filter
        .split('?')
        .enumerate()
        .map(|(i, part)| {
            if i == 0 {
                part.to_owned()
            } else {
                parameter += 1;
                format!("?{parameter}{part}")
            }
        })
        .collect::<String>();
    keyset_sql(
        query.runs,
        &kinds,
        query.after,
        query.limit,
        &filter,
        params,
    )
}

/// Scope-first keyset statement shared by the filtered candidate query and
/// the unfiltered bounded scan. `kinds` is already de-duplicated and `params`
/// already holds the numbered bindings used by `filter`.
fn keyset_sql(
    runs: &[String],
    kinds: &[IndexBlockKind],
    after: Option<CandidatePosition>,
    limit: usize,
    filter: &str,
    mut params: Vec<SqlValue>,
) -> (String, Vec<SqlValue>) {
    let after_id = after.map(|after| {
        params.push(SqlValue::Integer(after.block_id));
        params.len()
    });
    params.push(SqlValue::Integer(to_i64(limit)));
    let limit = params.len();
    let mut arms = Vec::new();
    for (seq, run) in runs.iter().enumerate() {
        if after.is_some_and(|after| seq < after.seq) {
            continue;
        }
        params.push(SqlValue::Text(run.clone()));
        let run_parameter = params.len();
        let after = if after.is_some_and(|after| seq == after.seq) {
            format!(" AND b.id > ?{}", after_id.unwrap())
        } else {
            String::new()
        };
        for kind in kinds {
            arms.push(format!(
                "SELECT b.id, {seq} AS seq, b.run_id, r.source_revision, r.snapshot_revision, \
                        b.source_kind, b.ordinal, b.block_index, b.note_seq, b.role, b.kind, \
                        b.tool_name, b.truncated, b.text \
                 FROM thread_index_runs r \
                 CROSS JOIN conversation_blocks b INDEXED BY idx_conversation_blocks_kind \
                     ON b.run_id = r.run_id \
                 WHERE r.run_id = ?{run_parameter} AND ({current}) \
                   AND r.run_id NOT IN (SELECT run_id FROM run_tombstones) \
                   AND b.kind = '{kind}'{after} AND ({filter})",
                kind = kind_text(*kind),
                current = current_index_sql(),
            ));
        }
    }
    if arms.is_empty() {
        // A cursor beyond the last seq is exhausted, including usize::MAX.
        return (
            format!(
                "SELECT b.id, 0 AS seq, b.run_id, 0, 0, \
                        b.source_kind, b.ordinal, b.block_index, b.note_seq, b.role, b.kind, \
                        b.tool_name, b.truncated, b.text \
                 FROM conversation_blocks b WHERE 0 AND ({filter}) LIMIT ?{limit}"
            ),
            params,
        );
    }
    // Five unique kinds * 100 runs is exactly SQLite's bundled default
    // SQLITE_LIMIT_COMPOUND_SELECT (500). Neither bound is expanded here.
    let sql = format!(
        "{} ORDER BY seq, id LIMIT ?{limit}",
        arms.join(" UNION ALL ")
    );
    (sql, params)
}

/// Statement for [`Database::conversation_scan_chunk`]: the raw keyset stream
/// with no text predicate, so no FTS table is consulted.
fn conversation_scan_sql(scan: &ConversationScan<'_>, limit: usize) -> (String, Vec<SqlValue>) {
    keyset_sql(
        scan.runs,
        &unique_kinds(scan.kinds),
        scan.after,
        limit,
        "1",
        Vec::new(),
    )
}

fn unique_kinds(kinds: &[IndexBlockKind]) -> Vec<IndexBlockKind> {
    let mut unique = Vec::new();
    for kind in kinds {
        if !unique.contains(kind) {
            unique.push(*kind);
        }
    }
    unique
}

fn check_batch(runs: &[String]) -> Result<(), StorageError> {
    if runs.is_empty() || runs.len() > MAX_SEARCH_BATCH_RUNS {
        return Err(StorageError::Serialization(format!(
            "conversation search batches hold 1-{MAX_SEARCH_BATCH_RUNS} runs"
        )));
    }
    Ok(())
}

fn scope_values(runs: &[String], params: &mut Vec<SqlValue>) -> String {
    runs.iter()
        .enumerate()
        .map(|(seq, run)| {
            params.push(SqlValue::Text(run.clone()));
            params.push(SqlValue::Integer(to_i64(seq)));
            "(?, ?)"
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// OR of alternatives, each an AND of per-term predicates. A term without a
/// SQL guarantee adds no predicate; an alternative without any makes the
/// whole filter a scan.
fn filter_sql(filter: &ConversationFilter, params: &mut Vec<SqlValue>, exact: bool) -> String {
    let ConversationFilter::Literals(alternatives) = filter else {
        return "1".into();
    };
    let mut clauses = Vec::new();
    let mut clause_params = Vec::new();
    for terms in alternatives {
        let mut phrases = Vec::new();
        let mut phrase_tokens = Vec::new();
        let mut likes = Vec::new();
        let mut like_params = Vec::new();
        for term in terms {
            match term_route(term) {
                TermRoute::Trigram(variants) => {
                    phrases.push(format!(
                        "({})",
                        variants
                            .iter()
                            .map(|variant| format!("\"{}\"", variant.replace('"', "\"\"")))
                            .collect::<Vec<_>>()
                            .join(" OR ")
                    ));
                    phrase_tokens.push(variants);
                }
                TermRoute::Like(variants) => {
                    let any = vec!["b.text LIKE ? ESCAPE '\\'"; variants.len()].join(" OR ");
                    likes.push(format!("({any})"));
                    like_params.extend(
                        variants
                            .iter()
                            .map(|variant| SqlValue::Text(format!("%{}%", like_escape(variant)))),
                    );
                }
                TermRoute::Scan => {}
            }
        }
        let mut predicates = Vec::new();
        if !phrases.is_empty() {
            let fallback = "EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?)";
            // Replace only this existing MATCH leaf, never the whole filter:
            // LIKE and unrestricted Scan alternatives keep their SQL meaning.
            // NULL (unknown) lazily evaluates the original rowid/MATCH probe.
            if let Some(json) = exact
                .then(|| serde_json::to_string(&vec![phrase_tokens]).ok())
                .flatten()
            {
                predicates.push(format!(
                    "COALESCE(evorch_fts5_exact(b.text, ?), {fallback})"
                ));
                clause_params.push(SqlValue::Text(json));
            } else {
                predicates.push(fallback.to_owned());
            }
            clause_params.push(SqlValue::Text(phrases.join(" AND ")));
        }
        predicates.extend(likes);
        clause_params.extend(like_params);
        if predicates.is_empty() {
            return "1".into();
        }
        clauses.push(predicates.join(" AND "));
    }
    params.extend(clause_params);
    clauses
        .into_iter()
        .map(|clause| format!("({clause})"))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Escape `LIKE` metacharacters for `ESCAPE '\'`.
fn like_escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn kind_text(kind: IndexBlockKind) -> &'static str {
    match kind {
        IndexBlockKind::Text => "text",
        IndexBlockKind::WorkingNote => "working_note",
        IndexBlockKind::ToolUse => "tool_use",
        IndexBlockKind::ToolResult => "tool_result",
        IndexBlockKind::ToolError => "tool_error",
    }
}

fn to_i64(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn invalid_column(index: usize, what: &str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        format!("unknown {what}").into(),
    )
}

fn candidate(row: &Row<'_>) -> rusqlite::Result<ConversationCandidate> {
    let source_kind: String = row.get(5)?;
    let locator = match source_kind.as_str() {
        "message" => BlockLocator::Message {
            ordinal: row.get(6)?,
            block_index: row.get(7)?,
        },
        "working_note" => BlockLocator::WorkingNote {
            note_seq: row.get(8)?,
        },
        _ => return Err(invalid_column(5, "source kind")),
    };
    let role = match row.get::<_, String>(9)?.as_str() {
        "user" => IndexRole::User,
        "assistant" => IndexRole::Assistant,
        "working_note" => IndexRole::WorkingNote,
        _ => return Err(invalid_column(9, "role")),
    };
    let kind = match row.get::<_, String>(10)?.as_str() {
        "text" => IndexBlockKind::Text,
        "working_note" => IndexBlockKind::WorkingNote,
        "tool_use" => IndexBlockKind::ToolUse,
        "tool_result" => IndexBlockKind::ToolResult,
        "tool_error" => IndexBlockKind::ToolError,
        _ => return Err(invalid_column(10, "kind")),
    };
    Ok(ConversationCandidate {
        position: CandidatePosition {
            seq: row.get(1)?,
            block_id: row.get(0)?,
        },
        run_id: row.get(2)?,
        source_revision: row.get(3)?,
        snapshot_revision: row.get(4)?,
        locator,
        role,
        kind,
        tool_name: row.get(11)?,
        truncated: row.get(12)?,
        text: row.get(13)?,
    })
}
