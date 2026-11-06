//! Snapshots: the shared facts about a pull request, ordered by the sequence handed out
//! when a fetch starts.

use rusqlite::{Connection, OptionalExtension, params};

use super::{enum_at, immediate, key_at, valid_commit_id};
use crate::StorageError;
use crate::pull_requests::{
    CheckStatus, DisplayTitle, FetchAttempt, FetchCommit, FetchErrorKind, FetchFailure,
    PULL_REQUEST_BRANCH_MAX_CHARS, PULL_REQUEST_CHECK_ID_MAX_CHARS,
    PULL_REQUEST_CHECK_NAME_MAX_CHARS, PULL_REQUEST_CHECKS_MAX, PULL_REQUEST_ERROR_MAX_CHARS,
    PullRequestCheck, PullRequestDisplay, PullRequestFacts, PullRequestKey, PullRequestSnapshot,
    PullRequestState, ReviewDecision, invalid, normalize_repository, sanitize_single_line,
};

const COLUMNS: &str = "provider, host, repository, number, state, base_branch, head_repository, \
    head_branch, head_sha, checks_json, checks_complete, review_decision, fetched_at_ns, \
    success_seq, fetch_seq, last_attempt_seq, last_attempt_at_ns, last_error_kind, \
    last_error_message, consecutive_failures, title";

const KEY_WHERE: &str = "provider = ?1 AND host = ?2 AND repository = ?3 AND number = ?4";

fn redact(text: &str) -> String {
    secret_guard::SecretRedactor::from_env().redact(text).text
}

/// A third-party single-line text: the whole original is redacted first, so a credential
/// that crosses the length limit is masked before anything is cut, and only then is the
/// text made single-line and bounded.
fn clean_text(text: &str, max_chars: usize) -> String {
    sanitize_single_line(&redact(text), max_chars)
}

fn branch(field: &'static str, value: &str) -> Result<String, StorageError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid(field, "must not be empty"));
    }
    if value.chars().count() > PULL_REQUEST_BRANCH_MAX_CHARS {
        return Err(invalid(field, "is too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(invalid(field, "must not contain control characters"));
    }
    Ok(value.to_owned())
}

/// Validates the facts of a fetch and returns them in stored form (checks sanitized).
fn clean_facts(facts: &PullRequestFacts) -> Result<PullRequestFacts, StorageError> {
    if facts.checks.len() > PULL_REQUEST_CHECKS_MAX {
        return Err(invalid(
            "checks",
            "too many checks; cut the list and report it incomplete",
        ));
    }
    let head_repository = facts
        .head_repository
        .as_deref()
        .map(|repository| normalize_repository("head_repository", repository))
        .transpose()?;
    let mut checks = Vec::with_capacity(facts.checks.len());
    for check in &facts.checks {
        let provider_id = clean_text(&check.provider_id, PULL_REQUEST_CHECK_ID_MAX_CHARS);
        let name = clean_text(&check.name, PULL_REQUEST_CHECK_NAME_MAX_CHARS);
        if provider_id.is_empty() || name.is_empty() {
            return Err(invalid("checks", "a check needs an id and a name"));
        }
        if (check.status == CheckStatus::Completed) != check.conclusion.is_some() {
            return Err(invalid(
                "checks",
                "a conclusion belongs to a completed check only",
            ));
        }
        checks.push(PullRequestCheck {
            provider_id,
            kind: check.kind,
            name,
            status: check.status,
            conclusion: check.conclusion,
            head_sha: valid_commit_id("checks.head_sha", &check.head_sha)?,
        });
    }
    Ok(PullRequestFacts {
        state: facts.state,
        base_branch: branch("base_branch", &facts.base_branch)?,
        head_repository,
        head_branch: branch("head_branch", &facts.head_branch)?,
        head_sha: valid_commit_id("head_sha", &facts.head_sha)?,
        checks,
        checks_complete: facts.checks_complete,
        review_decision: facts.review_decision,
    })
}

/// Hands out the sequence of a fetch that is starting. Strictly increasing per key
/// across every process; the row is created by the first call.
pub(crate) fn begin_fetch(conn: &Connection, key: &PullRequestKey) -> Result<u64, StorageError> {
    let key = key.normalized()?;
    Ok(conn.query_row(
        "INSERT INTO pull_request_snapshots(provider, host, repository, number, fetch_seq) \
         VALUES (?1, ?2, ?3, ?4, 1) \
         ON CONFLICT(provider, host, repository, number) \
            DO UPDATE SET fetch_seq = fetch_seq + 1 \
         RETURNING fetch_seq",
        params![key.provider.as_str(), key.host, key.repository, key.number],
        |row| row.get(0),
    )?)
}

/// The counters of a key a result is checked against, once `seq` is known to have been issued.
struct Counters {
    success: u64,
    attempt: u64,
    min_valid: u64,
}

impl Counters {
    /// A fetch that started before retention cleared the key has no say any more.
    fn invalidated(&self, seq: u64) -> bool {
        seq < self.min_valid
    }
}

fn counters(conn: &Connection, key: &PullRequestKey, seq: u64) -> Result<Counters, StorageError> {
    let row: Option<(u64, u64, u64, u64)> = conn
        .query_row(
            &format!(
                "SELECT success_seq, last_attempt_seq, min_valid_seq, fetch_seq \
                 FROM pull_request_snapshots WHERE {KEY_WHERE}"
            ),
            params![key.provider.as_str(), key.host, key.repository, key.number],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    match row {
        Some((success, attempt, min_valid, issued)) if seq >= 1 && seq <= issued => Ok(Counters {
            success,
            attempt,
            min_valid,
        }),
        _ => Err(invalid("seq", "was not handed out by begin_fetch")),
    }
}

const REFUSED: FetchCommit = FetchCommit {
    facts_stored: false,
    attempt_recorded: false,
};

pub(crate) fn commit_success(
    conn: &Connection,
    key: &PullRequestKey,
    seq: u64,
    facts: &PullRequestFacts,
    display: &PullRequestDisplay,
    at_ns: i64,
) -> Result<FetchCommit, StorageError> {
    let key = key.normalized()?;
    let facts = clean_facts(facts)?;
    // Cleaned again here: the caller's text is not trusted to have been.
    let title = display
        .title
        .as_ref()
        .and_then(|title| DisplayTitle::from_untrusted(title.as_str()));
    let checks_json = serde_json::to_string(&facts.checks)
        .map_err(|error| StorageError::Serialization(error.to_string()))?;
    let tx = immediate(conn)?;
    let counters = counters(&tx, &key, seq)?;
    if counters.invalidated(seq) {
        return Ok(REFUSED);
    }
    let facts_stored = seq > counters.success;
    let attempt_recorded = seq > counters.attempt;
    if facts_stored {
        tx.execute(
            &format!(
                "UPDATE pull_request_snapshots SET state = ?5, base_branch = ?6, \
                    head_repository = ?7, head_branch = ?8, head_sha = ?9, checks_json = ?10, \
                    checks_complete = ?11, review_decision = ?12, fetched_at_ns = ?13, \
                    success_seq = ?14, title = ?15 \
                 WHERE {KEY_WHERE} AND success_seq < ?14"
            ),
            params![
                key.provider.as_str(),
                key.host,
                key.repository,
                key.number,
                facts.state.as_str(),
                facts.base_branch,
                facts.head_repository,
                facts.head_branch,
                facts.head_sha,
                checks_json,
                facts.checks_complete,
                facts.review_decision.as_str(),
                at_ns,
                seq,
                title.as_ref().map(DisplayTitle::as_str),
            ],
        )?;
    }
    if attempt_recorded {
        tx.execute(
            &format!(
                "UPDATE pull_request_snapshots SET last_attempt_seq = ?5, last_attempt_at_ns = ?6, \
                    last_error_kind = NULL, last_error_message = NULL, consecutive_failures = 0 \
                 WHERE {KEY_WHERE} AND last_attempt_seq < ?5"
            ),
            params![
                key.provider.as_str(),
                key.host,
                key.repository,
                key.number,
                seq,
                at_ns,
            ],
        )?;
    }
    tx.commit()?;
    Ok(FetchCommit {
        facts_stored,
        attempt_recorded,
    })
}

pub(crate) fn commit_failure(
    conn: &Connection,
    key: &PullRequestKey,
    seq: u64,
    failure: &FetchFailure,
    at_ns: i64,
) -> Result<FetchCommit, StorageError> {
    let key = key.normalized()?;
    let mut message = clean_text(&failure.message, PULL_REQUEST_ERROR_MAX_CHARS);
    if message.is_empty() {
        message = failure.kind.as_str().to_owned();
    }
    let tx = immediate(conn)?;
    let counters = counters(&tx, &key, seq)?;
    if counters.invalidated(seq) {
        return Ok(REFUSED);
    }
    let attempt_recorded = seq > counters.attempt;
    if attempt_recorded {
        tx.execute(
            &format!(
                "UPDATE pull_request_snapshots SET last_attempt_seq = ?5, last_attempt_at_ns = ?6, \
                    last_error_kind = ?7, last_error_message = ?8, \
                    consecutive_failures = consecutive_failures + 1 \
                 WHERE {KEY_WHERE} AND last_attempt_seq < ?5"
            ),
            params![
                key.provider.as_str(),
                key.host,
                key.repository,
                key.number,
                seq,
                at_ns,
                failure.kind.as_str(),
                message,
            ],
        )?;
    }
    tx.commit()?;
    Ok(FetchCommit {
        facts_stored: false,
        attempt_recorded,
    })
}

fn bad_column(index: usize, name: &str) -> rusqlite::Error {
    rusqlite::Error::InvalidColumnType(index, name.into(), rusqlite::types::Type::Text)
}

pub(crate) fn get(
    conn: &Connection,
    key: &PullRequestKey,
) -> Result<Option<PullRequestSnapshot>, StorageError> {
    let key = key.normalized()?;
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM pull_request_snapshots WHERE {KEY_WHERE}"),
            params![key.provider.as_str(), key.host, key.repository, key.number],
            |row| {
                let key = key_at(row, 0)?;
                let state: Option<String> = row.get(4)?;
                let facts = match state {
                    None => None,
                    Some(state) => {
                        let checks_json: String = row.get(9)?;
                        let checks: Vec<PullRequestCheck> = serde_json::from_str(&checks_json)
                            .map_err(|error| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    9,
                                    rusqlite::types::Type::Text,
                                    Box::new(error),
                                )
                            })?;
                        Some(PullRequestFacts {
                            state: PullRequestState::parse(&state)
                                .ok_or_else(|| bad_column(4, "state"))?,
                            base_branch: row.get(5)?,
                            head_repository: row.get(6)?,
                            head_branch: row.get(7)?,
                            head_sha: row.get(8)?,
                            checks,
                            checks_complete: row.get(10)?,
                            review_decision: enum_at(
                                row,
                                11,
                                "review decision",
                                ReviewDecision::parse,
                            )?,
                        })
                    }
                };
                let error_kind: Option<String> = row.get(17)?;
                let failure = match error_kind {
                    None => None,
                    Some(kind) => Some(FetchFailure {
                        kind: FetchErrorKind::parse(&kind)
                            .ok_or_else(|| bad_column(17, "error kind"))?,
                        message: row.get(18)?,
                    }),
                };
                let attempt_at: Option<i64> = row.get(16)?;
                let last_attempt = match attempt_at {
                    None => None,
                    Some(at_ns) => Some(FetchAttempt {
                        seq: row.get(15)?,
                        at_ns,
                        failure,
                    }),
                };
                let title: Option<String> = row.get(20)?;
                Ok(PullRequestSnapshot {
                    key,
                    facts,
                    display: PullRequestDisplay {
                        title: title.as_deref().and_then(DisplayTitle::from_untrusted),
                    },
                    fetched_at_ns: row.get(12)?,
                    success_seq: row.get(13)?,
                    fetch_seq: row.get(14)?,
                    last_attempt,
                    consecutive_failures: row.get(19)?,
                })
            },
        )
        .optional()?)
}

/// Clears the cached facts and the error of up to `limit` snapshots that only `thread`
/// references (links, candidates or inheritance rows), and moves their `min_valid_seq`
/// past every sequence handed out so far: the result of a fetch that was still running
/// (a success or a failure, with or without earlier facts) is refused afterwards. The
/// sequence counters stay, so a later fetch always outranks the cleared ones. Returns the
/// rows cleared; it is idempotent, because a cleared snapshot no longer matches.
pub(crate) fn clear_unshared(
    conn: &Connection,
    thread: &str,
    limit: i64,
) -> Result<u64, StorageError> {
    let cleared = conn.execute(
        "UPDATE pull_request_snapshots SET state = NULL, base_branch = NULL, \
            head_repository = NULL, head_branch = NULL, head_sha = NULL, checks_json = '[]', \
            checks_complete = 0, review_decision = NULL, title = NULL, fetched_at_ns = NULL, \
            last_attempt_at_ns = NULL, last_error_kind = NULL, last_error_message = NULL, \
            consecutive_failures = 0, min_valid_seq = fetch_seq + 1 \
         WHERE (provider, host, repository, number) IN (\
            SELECT s.provider, s.host, s.repository, s.number FROM pull_request_snapshots AS s \
            WHERE (s.state IS NOT NULL OR s.last_attempt_at_ns IS NOT NULL \
                   OR s.fetch_seq >= s.min_valid_seq) \
              AND (s.provider, s.host, s.repository, s.number) IN (\
                  SELECT provider, host, repository, number FROM thread_pull_requests \
                  WHERE thread_id = ?1 \
                  UNION SELECT provider, host, repository, number \
                  FROM thread_pull_request_candidates WHERE thread_id = ?1 \
                  UNION SELECT provider, host, repository, number \
                  FROM thread_pull_request_inheritance WHERE child_thread_id = ?1) \
              AND NOT EXISTS (SELECT 1 FROM thread_pull_requests AS l WHERE l.provider = s.provider \
                  AND l.host = s.host AND l.repository = s.repository AND l.number = s.number \
                  AND l.thread_id <> ?1) \
              AND NOT EXISTS (SELECT 1 FROM thread_pull_request_candidates AS c \
                  WHERE c.provider = s.provider AND c.host = s.host \
                  AND c.repository = s.repository AND c.number = s.number AND c.thread_id <> ?1) \
              AND NOT EXISTS (SELECT 1 FROM thread_pull_request_inheritance AS i \
                  WHERE i.provider = s.provider AND i.host = s.host \
                  AND i.repository = s.repository AND i.number = s.number \
                  AND i.child_thread_id <> ?1) \
            LIMIT ?2)",
        params![thread, limit],
    )?;
    Ok(cleared as u64)
}

#[cfg(test)]
#[path = "snapshots_tests.rs"]
mod tests;
