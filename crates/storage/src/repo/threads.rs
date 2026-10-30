//! Thread rows, run bindings and the thread-scoped reads built on them.

use std::collections::HashSet;

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::threads::{
    MAX_RUN_LINKS, MAX_THREAD_PAGE, RunTerminalSummary, ThreadCursor, ThreadLineageKind,
    ThreadPage, ThreadRow, ThreadRunRow, ThreadSource, ThreadUpsert, ThreadWriter,
    parse_chat_run_name,
};
use crate::{RunContextRecord, RunLedgerEntry, StorageError};

const COLUMNS: &str = "thread_id, project_id, project_root, title, root_run_id, chat_role, \
                       parent_thread_id, lineage_kind, fork_point_run_id, \
                       fork_point_context_len, escalation_source_run_id, archived, superseded, \
                       created_at_ns, updated_at_ns, source";

/// Nullable definition columns merged one by one, in parameter order from `?2`.
const MERGED: [&str; 10] = [
    "project_id",
    "project_root",
    "title",
    "root_run_id",
    "chat_role",
    "parent_thread_id",
    "lineage_kind",
    "fork_point_run_id",
    "fork_point_context_len",
    "escalation_source_run_id",
];

/// The host replaces what it supplies. The runtime only fills what a host row
/// left empty, and replaces its own or backfilled values. `NULL` never erases.
fn upsert_sql(writer: ThreadWriter) -> String {
    let (merge, flag, created_at, source) = match writer {
        ThreadWriter::Host => (
            "{c} = COALESCE(excluded.{c}, threads.{c})",
            "{c} = COALESCE({p}, threads.{c})",
            "CASE WHEN excluded.created_at_ns > 0 THEN excluded.created_at_ns \
             ELSE threads.created_at_ns END",
            "'host'",
        ),
        ThreadWriter::Runtime => (
            "{c} = CASE WHEN threads.source = 'host' THEN COALESCE(threads.{c}, excluded.{c}) \
             ELSE COALESCE(excluded.{c}, threads.{c}) END",
            "{c} = CASE WHEN threads.source = 'host' OR {p} IS NULL THEN threads.{c} ELSE {p} END",
            "CASE WHEN threads.created_at_ns > 0 THEN threads.created_at_ns \
             ELSE excluded.created_at_ns END",
            "CASE WHEN threads.source = 'host' THEN 'host' ELSE 'runtime' END",
        ),
    };
    let mut updates: Vec<String> = MERGED
        .iter()
        .map(|column| merge.replace("{c}", column))
        .collect();
    for (column, parameter) in [("archived", "?12"), ("superseded", "?13")] {
        updates.push(flag.replace("{c}", column).replace("{p}", parameter));
    }
    updates.push(format!("created_at_ns = {created_at}"));
    updates.push("updated_at_ns = max(threads.updated_at_ns, excluded.updated_at_ns)".into());
    updates.push(format!("source = {source}"));
    let inserted_source = match writer {
        ThreadWriter::Host => ThreadSource::Host,
        ThreadWriter::Runtime => ThreadSource::Runtime,
    };
    format!(
        "INSERT INTO threads ({COLUMNS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, \
         COALESCE(?12, 0), COALESCE(?13, 0), ?14, ?15, '{}') \
         ON CONFLICT(thread_id) DO UPDATE SET {}",
        inserted_source.as_str(),
        updates.join(", ")
    )
}

/// Write a batch atomically; one invalid definition rejects the whole batch.
pub(crate) fn upsert(
    conn: &Connection,
    writer: ThreadWriter,
    threads: &[ThreadUpsert],
) -> Result<(), StorageError> {
    if threads.is_empty() {
        return Ok(());
    }
    threads.iter().try_for_each(ThreadUpsert::validate)?;
    let redactor = secret_guard::SecretRedactor::from_env();
    let transaction = conn.unchecked_transaction()?;
    {
        let mut statement = transaction.prepare(&upsert_sql(writer))?;
        for thread in threads {
            let title = thread
                .title
                .as_deref()
                .map(|title| redactor.redact(title).text);
            let fork_point_context_len = thread
                .fork_point_context_len
                .map(|length| {
                    i64::try_from(length).map_err(|_| StorageError::OutOfRange("fork point length"))
                })
                .transpose()?;
            statement.execute(params![
                thread.thread_id,
                thread.project_id,
                thread.project_root,
                title,
                thread.root_run_id,
                thread.chat_role,
                thread.parent_thread_id,
                thread.lineage_kind.map(ThreadLineageKind::as_str),
                thread.fork_point_run_id,
                fork_point_context_len,
                thread.escalation_source_run_id,
                thread.archived,
                thread.superseded,
                thread.created_at_ns,
                thread.updated_at_ns,
            ])?;
        }
    }
    transaction.commit()?;
    Ok(())
}

pub(crate) fn get(conn: &Connection, thread_id: &str) -> Result<Option<ThreadRow>, StorageError> {
    Ok(conn
        .query_row(
            &format!("SELECT {COLUMNS} FROM threads WHERE thread_id = ?1"),
            [thread_id],
            thread_from_row,
        )
        .optional()?)
}

pub(crate) fn in_project(
    conn: &Connection,
    project_root: &str,
    cursor: Option<&ThreadCursor>,
    limit: usize,
) -> Result<ThreadPage, StorageError> {
    let limit = limit.clamp(1, MAX_THREAD_PAGE);
    let fetch = i64::try_from(limit + 1).map_err(|_| StorageError::OutOfRange("thread page"))?;
    let mut threads = match cursor {
        None => {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM threads WHERE project_root = ?1 \
                 ORDER BY updated_at_ns DESC, thread_id DESC LIMIT ?2"
            ))?;
            statement
                .query_map(params![project_root, fetch], thread_from_row)?
                .collect::<Result<Vec<_>, _>>()?
        }
        Some(cursor) => {
            let mut statement = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM threads WHERE project_root = ?1 \
                 AND (updated_at_ns < ?2 OR (updated_at_ns = ?2 AND thread_id < ?3)) \
                 ORDER BY updated_at_ns DESC, thread_id DESC LIMIT ?4"
            ))?;
            statement
                .query_map(
                    params![project_root, cursor.updated_at_ns, cursor.thread_id, fetch],
                    thread_from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?
        }
    };
    let next_cursor = if threads.len() > limit {
        threads.truncate(limit);
        threads.last().map(|last| ThreadCursor {
            updated_at_ns: last.updated_at_ns,
            thread_id: last.thread_id.clone(),
        })
    } else {
        None
    };
    Ok(ThreadPage {
        threads,
        next_cursor,
    })
}

pub(crate) fn children(conn: &Connection, thread_id: &str) -> Result<Vec<ThreadRow>, StorageError> {
    let mut statement = conn.prepare(&format!(
        "SELECT {COLUMNS} FROM threads WHERE thread_id <> ?1 AND (parent_thread_id = ?1 \
         OR escalation_source_run_id IN (SELECT run_id FROM thread_runs WHERE thread_id = ?1)) \
         ORDER BY created_at_ns, thread_id"
    ))?;
    let rows = statement.query_map([thread_id], thread_from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn thread_of_run(
    conn: &Connection,
    run_id: &str,
) -> Result<Option<ThreadRunRow>, StorageError> {
    let transaction = conn.unchecked_transaction()?;
    let walk = walk(&transaction, run_id)?;
    transaction.commit()?;
    match walk {
        Walk::Thread(row) => Ok(Some(row)),
        Walk::Top { .. } => Ok(None),
        Walk::Broken => Err(StorageError::Serialization(format!(
            "run parent chain has a cycle or exceeds {MAX_RUN_LINKS} links"
        ))),
    }
}

pub(crate) fn runs_of_thread(
    conn: &Connection,
    thread_id: &str,
) -> Result<Vec<ThreadRunRow>, StorageError> {
    // Seeds mirror `walk`: bindings, parentless runs named for the thread and
    // the recorded root. Descendants follow both kinds of parent link.
    let mut statement = conn.prepare(
        "WITH RECURSIVE seed(run_id, root_run_id, depth) AS (\
             SELECT run_id, root_run_id, depth FROM thread_runs WHERE thread_id = ?1 \
             UNION SELECT run_id, run_id, 0 FROM run_contexts \
             WHERE parent_run_id IS NULL AND substr(name, 1, 5) = 'chat:' \
             AND instr(substr(name, 6), ':') > 1 \
             AND substr(name, 6 + instr(substr(name, 6), ':')) = ?1 \
             UNION SELECT root_run_id, root_run_id, 0 FROM threads \
             WHERE thread_id = ?1 AND root_run_id IS NOT NULL\
         ), tree(run_id, root_run_id, depth) AS (\
             SELECT run_id, root_run_id, depth FROM seed \
             UNION SELECT c.run_id, tree.root_run_id, tree.depth + 1 \
             FROM run_contexts c JOIN tree ON c.parent_run_id = tree.run_id \
             WHERE tree.depth < ?2 \
             UNION SELECT u.run_id, tree.root_run_id, tree.depth + 1 \
             FROM usage_requests u JOIN tree ON u.parent_run_id = tree.run_id \
             WHERE u.run_id IS NOT NULL AND tree.depth < ?2\
         ) \
         SELECT run_id, root_run_id, MIN(depth) AS depth FROM tree \
         GROUP BY run_id ORDER BY depth, run_id",
    )?;
    let rows = statement.query_map(params![thread_id, MAX_RUN_LINKS], |row| {
        Ok(ThreadRunRow {
            run_id: row.get(0)?,
            thread_id: thread_id.to_owned(),
            root_run_id: row.get(1)?,
            depth: depth_from(row, 2)?,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn run_contexts_for(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<RunContextRecord>, StorageError> {
    // No json_each column shares a name with a run_contexts column.
    let mut statement = conn.prepare(&format!(
        "SELECT {} FROM json_each(?1) AS requested \
         JOIN run_contexts ON run_contexts.run_id = requested.value ORDER BY requested.key",
        crate::repo::run_context::COLUMNS
    ))?;
    let rows = statement.query_map(
        [distinct_json(run_ids)?],
        crate::repo::run_context::from_row,
    )?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn latest_ledgers(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<RunLedgerEntry>, StorageError> {
    let mut statement = conn.prepare(
        "SELECT seq, run_id, body, created_at_ns FROM run_ledger WHERE seq IN (\
         SELECT MAX(seq) FROM run_ledger \
         WHERE run_id IN (SELECT value FROM json_each(?1)) GROUP BY run_id) \
         ORDER BY seq",
    )?;
    let rows = statement.query_map([distinct_json(run_ids)?], crate::repo::run_ledger::from_row)?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(crate) fn terminal_summaries(
    conn: &Connection,
    run_ids: &[String],
) -> Result<Vec<RunTerminalSummary>, StorageError> {
    // JSON is decoded inside SQLite so only the last message leaves it. A
    // compressed snapshot cannot be, so its bytes are returned and decoded here.
    let mut statement = conn.prepare(
        "SELECT c.run_id, c.role, c.name, c.parent_run_id, c.terminal_phase, c.restorable, \
         c.updated_at_ns, \
         CASE WHEN json_valid(c.config_json) THEN \
             CASE WHEN json_type(c.config_json, '$.non_restorable_reason') = 'text' \
             THEN json_extract(c.config_json, '$.non_restorable_reason') END END, \
         CASE WHEN typeof(c.messages_json) = 'text' AND json_valid(c.messages_json) \
             THEN json_array_length(c.messages_json) END, \
         CASE WHEN typeof(c.messages_json) = 'text' AND json_valid(c.messages_json) \
             THEN json_extract(c.messages_json, '$[#-1]') END, \
         CASE WHEN typeof(c.messages_json) = 'blob' THEN c.messages_json END \
         FROM json_each(?1) AS requested JOIN run_contexts c ON c.run_id = requested.value \
         ORDER BY requested.key",
    )?;
    let rows = statement.query_map([distinct_json(run_ids)?], |row| {
        let mut message_count: Option<i64> = row.get(8)?;
        let mut last_message: Option<String> = row.get(9)?;
        if let rusqlite::types::ValueRef::Blob(_) = row.get_ref(10)? {
            let messages = crate::codec::text_column(row, 10)?;
            if let Ok(serde_json::Value::Array(messages)) = serde_json::from_str(&messages) {
                message_count = i64::try_from(messages.len()).ok();
                last_message = messages.last().map(serde_json::Value::to_string);
            }
        }
        Ok(RunTerminalSummary {
            run_id: row.get(0)?,
            role: row.get(1)?,
            name: row.get(2)?,
            parent_run_id: row.get(3)?,
            terminal_phase: row.get(4)?,
            restorable: row.get(5)?,
            updated_at_ns: row.get(6)?,
            non_restorable_reason: row.get(7)?,
            message_count: message_count.map_or(0, |count| count.max(0).unsigned_abs()),
            final_text: last_message.as_deref().and_then(assistant_text),
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Keep `thread_runs` current when a context is saved. An explicit
/// `config_json.thread_id` replaces the binding; otherwise a run inherits its
/// parent's thread, or a parentless `chat:<Role>:<thread>` run roots it, without
/// replacing an existing binding. Runs under a broken parent chain stay unbound.
pub(crate) fn bind_saved_run(
    conn: &Connection,
    record: &RunContextRecord,
) -> Result<(), StorageError> {
    let explicit = configured_thread(&record.config_json);
    let parent = record.parent_run_id.as_deref();
    let (binding, replace) = match (explicit, parent) {
        (Some(thread_id), None) => ((thread_id, record.run_id.clone(), 0), true),
        (Some(thread_id), Some(parent)) => match walk(conn, parent)? {
            Walk::Thread(row) => ((thread_id, row.root_run_id, row.depth + 1), true),
            Walk::Top { root_run_id, hops } => ((thread_id, root_run_id, hops + 1), true),
            Walk::Broken => {
                warn_unbound(&record.run_id);
                return Ok(());
            }
        },
        (None, Some(parent)) => match walk(conn, parent)? {
            Walk::Thread(row) => ((row.thread_id, row.root_run_id, row.depth + 1), false),
            Walk::Top { .. } => return Ok(()),
            Walk::Broken => {
                warn_unbound(&record.run_id);
                return Ok(());
            }
        },
        (None, None) => match parse_chat_run_name(&record.name) {
            Some((_, thread_id)) => ((thread_id.to_owned(), record.run_id.clone(), 0), false),
            None => return Ok(()),
        },
    };
    let (thread_id, root_run_id, depth) = binding;
    let sql = if replace {
        "INSERT INTO thread_runs (run_id, thread_id, root_run_id, depth) VALUES (?1, ?2, ?3, ?4) \
         ON CONFLICT(run_id) DO UPDATE SET thread_id = excluded.thread_id, \
         root_run_id = excluded.root_run_id, depth = excluded.depth"
    } else {
        "INSERT OR IGNORE INTO thread_runs (run_id, thread_id, root_run_id, depth) \
         VALUES (?1, ?2, ?3, ?4)"
    };
    conn.execute(sql, params![record.run_id, thread_id, root_run_id, depth])?;
    Ok(())
}

fn warn_unbound(run_id: &str) {
    tracing::warn!(run_id, "run parent chain is broken; thread binding skipped");
}

fn configured_thread(config_json: &str) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct Binding {
        #[serde(default)]
        thread_id: Option<String>,
    }
    serde_json::from_str::<Binding>(config_json)
        .ok()?
        .thread_id
        .filter(|thread_id| !thread_id.is_empty())
}

/// Where a run's ancestry leads.
enum Walk {
    /// A thread was found at the run or one of its ancestors.
    Thread(ThreadRunRow),
    /// The topmost ancestor names no thread.
    Top { root_run_id: String, hops: u32 },
    /// The chain has a cycle or exceeds [`MAX_RUN_LINKS`].
    Broken,
}

/// Follow parent links from `run_id`. At each run: its `thread_runs` binding;
/// if it has no parent, its `chat:<Role>:<thread>` name, then a thread recording
/// it as root; otherwise its parent.
fn walk(conn: &Connection, run_id: &str) -> Result<Walk, StorageError> {
    let mut current = run_id.to_owned();
    let mut seen = HashSet::new();
    for hops in 0..=MAX_RUN_LINKS {
        if !seen.insert(current.clone()) {
            return Ok(Walk::Broken);
        }
        let bound = conn
            .query_row(
                "SELECT thread_id, root_run_id, depth FROM thread_runs WHERE run_id = ?1",
                [&current],
                |row| Ok((row.get::<_, String>(0)?, row.get(1)?, depth_from(row, 2)?)),
            )
            .optional()?;
        if let Some((thread_id, root_run_id, depth)) = bound {
            return Ok(Walk::Thread(ThreadRunRow {
                run_id: run_id.to_owned(),
                thread_id,
                root_run_id,
                depth: depth.saturating_add(hops),
            }));
        }
        let (name, parent) = identity(conn, &current)?;
        let Some(parent) = parent else {
            let named = name
                .as_deref()
                .and_then(parse_chat_run_name)
                .map(|(_, thread_id)| thread_id.to_owned());
            let thread_id = match named {
                Some(thread_id) => Some(thread_id),
                None => conn
                    .query_row(
                        "SELECT thread_id FROM threads WHERE root_run_id = ?1 \
                         ORDER BY updated_at_ns DESC, thread_id LIMIT 1",
                        [&current],
                        |row| row.get(0),
                    )
                    .optional()?,
            };
            return Ok(match thread_id {
                Some(thread_id) => Walk::Thread(ThreadRunRow {
                    run_id: run_id.to_owned(),
                    thread_id,
                    root_run_id: current,
                    depth: hops,
                }),
                None => Walk::Top {
                    root_run_id: current,
                    hops,
                },
            });
        };
        current = parent;
    }
    Ok(Walk::Broken)
}

/// A run's saved name and parent; an unsaved run's parent comes from the usage ledger.
fn identity(
    conn: &Connection,
    run_id: &str,
) -> Result<(Option<String>, Option<String>), StorageError> {
    let saved = conn
        .query_row(
            "SELECT name, parent_run_id FROM run_contexts WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((name, Some(parent))) = saved {
        return Ok((Some(name), Some(parent)));
    }
    let ledger_parent = conn
        .query_row(
            "SELECT parent_run_id FROM usage_requests \
             WHERE run_id = ?1 AND parent_run_id IS NOT NULL LIMIT 1",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok((saved.map(|(name, _)| name), ledger_parent))
}

fn distinct_json(run_ids: &[String]) -> Result<String, StorageError> {
    let mut seen = HashSet::new();
    let ids: Vec<&String> = run_ids.iter().filter(|id| seen.insert(*id)).collect();
    serde_json::to_string(&ids).map_err(|error| StorageError::Serialization(error.to_string()))
}

/// Text blocks of a message the assistant wrote.
fn assistant_text(message: &str) -> Option<String> {
    let message: serde_json::Value = serde_json::from_str(message).ok()?;
    if message.get("role")?.as_str()? != "assistant" {
        return None;
    }
    let texts: Vec<&str> = message
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|block| match block.get("type")?.as_str()? {
            "text" => block.get("text")?.as_str(),
            "anthropic" => {
                let block = block.get("block")?;
                (block.get("type")?.as_str()? == "text")
                    .then(|| block.get("text")?.as_str())
                    .flatten()
            }
            _ => None,
        })
        .collect();
    (!texts.is_empty()).then(|| texts.join("\n"))
}

fn depth_from(row: &Row<'_>, index: usize) -> rusqlite::Result<u32> {
    let depth: i64 = row.get(index)?;
    u32::try_from(depth).map_err(|_| rusqlite::Error::IntegralValueOutOfRange(index, depth))
}

fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<ThreadRow> {
    let invalid = |index: usize, column: &str| {
        rusqlite::Error::InvalidColumnType(index, column.into(), rusqlite::types::Type::Text)
    };
    let lineage_kind: Option<String> = row.get(7)?;
    let lineage_kind = lineage_kind
        .map(|label| ThreadLineageKind::parse(&label).ok_or_else(|| invalid(7, "lineage_kind")))
        .transpose()?;
    let source: String = row.get(15)?;
    let source = ThreadSource::parse(&source).ok_or_else(|| invalid(15, "source"))?;
    let fork_point_context_len: Option<i64> = row.get(9)?;
    Ok(ThreadRow {
        thread_id: row.get(0)?,
        project_id: row.get(1)?,
        project_root: row.get(2)?,
        title: row.get(3)?,
        root_run_id: row.get(4)?,
        chat_role: row.get(5)?,
        parent_thread_id: row.get(6)?,
        lineage_kind,
        fork_point_run_id: row.get(8)?,
        fork_point_context_len: fork_point_context_len
            .map(|length| {
                u64::try_from(length)
                    .map_err(|_| rusqlite::Error::IntegralValueOutOfRange(9, length))
            })
            .transpose()?,
        escalation_source_run_id: row.get(10)?,
        archived: row.get(11)?,
        superseded: row.get(12)?,
        created_at_ns: row.get(13)?,
        updated_at_ns: row.get(14)?,
        source,
    })
}

#[cfg(test)]
#[path = "threads_tests.rs"]
mod tests;
