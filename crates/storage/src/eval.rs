use serde::{Deserialize, Serialize};

use crate::{Database, StorageError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attribution {
    Planner,
    Worker,
    Reviewer,
    Synthesizer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureAttribution {
    OutputMismatch,
    Provider,
    Timeout,
    BudgetExceeded,
    MissingUsage,
    IncompleteResponse,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalTrace {
    pub id: String,
    pub arena_id: String,
    pub project: String,
    pub task_id: String,
    pub task_spec: String,
    pub config_id: String,
    pub profile: String,
    pub model: String,
    pub attribution: Attribution,
    pub output: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub elapsed_ms: u64,
    pub failure: Option<FailureAttribution>,
}

impl Database {
    pub fn eval_traces(&self, project: &str) -> Result<Vec<EvalTrace>, StorageError> {
        let mut statement = self.conn.prepare(
            "SELECT content FROM memory_ledger WHERE kind='eval_trace' AND project=?1 ORDER BY seq",
        )?;
        statement
            .query_map([project], |row| row.get::<_, String>(0))?
            .map(|row| {
                serde_json::from_str(&row?).map_err(|e| StorageError::Serialization(e.to_string()))
            })
            .collect()
    }
}

pub(crate) fn append(conn: &rusqlite::Connection, trace: &EvalTrace) -> Result<(), StorageError> {
    if [
        &trace.id,
        &trace.arena_id,
        &trace.project,
        &trace.task_id,
        &trace.task_spec,
        &trace.config_id,
        &trace.profile,
        &trace.model,
    ]
    .iter()
    .any(|v| v.trim().is_empty())
    {
        return Err(StorageError::Serialization(
            "eval trace requires identity and task spec".into(),
        ));
    }
    let content =
        serde_json::to_string(trace).map_err(|e| StorageError::Serialization(e.to_string()))?;
    crate::entity::SecretGuard::from_env().check_text("memory", "eval_trace", &content)?;
    conn.execute(
        "INSERT INTO memory_ledger(entry_id,project,task_id,content,evidence,status,kind) VALUES(?1,?2,?3,?4,?5,'candidate','eval_trace')",
        rusqlite::params![trace.id, trace.project, trace.task_id, content, trace.arena_id],
    )?;
    Ok(())
}
