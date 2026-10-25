//! ADR 0041 Phase B conversation search reads (order 6).
//!
//! The caller authorizes first and passes only runs it may read, at most
//! [`MAX_SEARCH_BATCH_RUNS`] per query. That is a per-query batch bound, not a
//! total bound: callers page through any number of runs batch by batch. Every
//! statement is scoped to the given runs before anything is returned, so the
//! rows, their order and their keyset positions never depend on other runs.
//!
//! Text filters are a *superset* prefilter. Callers must verify each candidate
//! with their own matcher; [`term_route`] only promises that no block a
//! case-insensitive substring match would accept is filtered out. Blocks are
//! returned only while their run is `indexed` at its current `source_revision`,
//! so a stale index never contributes a hit.

use rusqlite::types::Value as SqlValue;
use rusqlite::{Row, params_from_iter};

use crate::thread_index::{BlockLocator, IndexBlockKind, IndexRole};
use crate::{Database, StorageError};

#[cfg(test)]
mod tests;

/// Largest number of runs one statement is scoped to.
pub const MAX_SEARCH_BATCH_RUNS: usize = 100;

/// Largest number of candidate rows one statement returns.
pub const MAX_SEARCH_PAGE_ROWS: usize = 500;

/// Case-variant spellings one term may expand to before it falls back to a
/// scan; see [`term_route`].
pub const MAX_TERM_VARIANTS: usize = 16;

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
        let mut params: Vec<SqlValue> = Vec::new();
        let scope = scope_values(query.runs, &mut params);
        let kinds = query
            .kinds
            .iter()
            .map(|kind| {
                params.push(SqlValue::Text(kind_text(*kind).into()));
                "?"
            })
            .collect::<Vec<_>>()
            .join(", ");
        let (after_seq, after_id) = query
            .after
            .map_or((-1, 0), |after| (to_i64(after.seq), after.block_id));
        params.push(SqlValue::Integer(after_seq));
        params.push(SqlValue::Integer(after_id));
        let filter = filter_sql(query.filter, &mut params);
        params.push(SqlValue::Integer(to_i64(query.limit)));
        let sql = format!(
            "WITH scope(run_id, seq) AS (VALUES {scope}) \
             SELECT b.id, scope.seq, b.run_id, r.source_revision, r.snapshot_revision, \
                    b.source_kind, b.ordinal, b.block_index, b.note_seq, b.role, b.kind, \
                    b.tool_name, b.truncated, b.text \
             FROM scope \
             JOIN thread_index_runs r ON r.run_id = scope.run_id \
             JOIN conversation_blocks b ON b.run_id = scope.run_id \
             WHERE r.state = 'indexed' AND r.source_present = 1 \
               AND r.indexed_revision = r.source_revision \
               AND b.kind IN ({kinds}) \
               AND (scope.seq, b.id) > (?, ?) \
               AND ({filter}) \
             ORDER BY scope.seq, b.id \
             LIMIT ?"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), candidate)?;
        Ok(rows.collect::<Result<_, _>>()?)
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
                    r.indexed_revision \
             FROM scope LEFT JOIN thread_index_runs r ON r.run_id = scope.run_id \
             ORDER BY scope.seq"
        );
        let mut statement = self.conn.prepare(&sql)?;
        let rows = statement.query_map(params_from_iter(params), |row| {
            let state: Option<String> = row.get(1)?;
            let present: Option<bool> = row.get(2)?;
            let source: Option<u64> = row.get(3)?;
            let indexed: Option<u64> = row.get(4)?;
            let coverage = match (state.as_deref(), present) {
                (None, _) => RunCoverage::Undiscovered,
                (Some("deleted"), _) | (_, Some(false)) => RunCoverage::Deleted,
                (Some("evicted"), _) => RunCoverage::Evicted,
                (Some("blocked"), _) => RunCoverage::Blocked,
                (Some("retry_wait"), _) => RunCoverage::RetryWait,
                (Some("indexed"), _) if indexed.is_some() && indexed == source => {
                    RunCoverage::Indexed
                }
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
fn filter_sql(filter: &ConversationFilter, params: &mut Vec<SqlValue>) -> String {
    let ConversationFilter::Literals(alternatives) = filter else {
        return "1".into();
    };
    let mut clauses = Vec::new();
    let mut clause_params = Vec::new();
    for terms in alternatives {
        let mut phrases = Vec::new();
        let mut likes = Vec::new();
        let mut like_params = Vec::new();
        for term in terms {
            match term_route(term) {
                TermRoute::Trigram(variants) => phrases.push(format!(
                    "({})",
                    variants
                        .iter()
                        .map(|variant| format!("\"{}\"", variant.replace('"', "\"\"")))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                )),
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
            predicates.push(
                "EXISTS (SELECT 1 FROM conversation_fts WHERE rowid = b.id AND conversation_fts MATCH ?)"
                    .to_owned(),
            );
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
