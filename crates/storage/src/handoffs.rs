//! Durable, idempotent Worker-to-lane handoff operations.
use crate::{StorageError, StorageHandle};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HandoffState {
    Preparing,
    Prepared,
    Committed,
    Started,
    RecoverableStartFailure,
}
impl HandoffState {
    pub fn is_committed(self) -> bool {
        matches!(
            self,
            Self::Committed | Self::Started | Self::RecoverableStartFailure
        )
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandoffReceipt {
    pub source_run_id: String,
    pub source_incarnation: String,
    pub target_run_id: String,
    pub lane_id: String,
    pub request_json: String,
    pub state: HandoffState,
    pub goal_json: Option<String>,
    pub todo_json: Option<String>,
}
const COLUMNS: &str =
    "source_run_id,source_incarnation,target_run_id,lane_id,request_json,state,goal_json,todo_json";
fn decode(r: &rusqlite::Row<'_>) -> rusqlite::Result<HandoffReceipt> {
    Ok(HandoffReceipt {
        source_run_id: r.get(0)?,
        source_incarnation: r.get(1)?,
        target_run_id: r.get(2)?,
        lane_id: r.get(3)?,
        request_json: r.get(4)?,
        state: serde_json::from_value(serde_json::Value::String(r.get(5)?)).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e))
        })?,
        goal_json: r.get(6)?,
        todo_json: r.get(7)?,
    })
}
fn operation(
    conn: &rusqlite::Connection,
    source: &str,
    incarnation: &str,
) -> Result<Option<HandoffReceipt>, StorageError> {
    Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM lane_handoffs WHERE source_run_id=?1 AND source_incarnation=?2"),params![source,incarnation],decode).optional()?)
}
#[derive(Debug, Clone)]
pub struct HandoffQuestionOwner {
    pub source_run_id: String,
    pub source_incarnation: String,
    pub target_run_id: String,
    pub committed: bool,
}
fn question_owner(
    conn: &rusqlite::Connection,
    id: &str,
) -> Result<Option<HandoffQuestionOwner>, StorageError> {
    Ok(conn.query_row("SELECT source_run_id,source_incarnation,target_run_id,committed FROM lane_handoff_questions WHERE question_id=?1",[id],|r|Ok(HandoffQuestionOwner{source_run_id:r.get(0)?,source_incarnation:r.get(1)?,target_run_id:r.get(2)?,committed:r.get(3)?})).optional()?)
}
impl StorageHandle {
    /// Replaying an incarnation returns its target. A new human incarnation gets
    /// a new operation; no other incarnation can replace its target.
    pub fn reserve_handoff(&self, receipt: HandoffReceipt) -> Result<HandoffReceipt, StorageError> {
        self.lane_job(true, move |conn| {
            let deleted: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)",[&receipt.source_run_id],|r|r.get(0))?;
            if deleted {return Err(StorageError::Tombstoned {run_id: Some(receipt.source_run_id.clone()),thread_id:None});}
            crate::entity::SecretGuard::from_env().check_text("handoff", "request", &receipt.request_json)?;
            let tx = conn.unchecked_transaction()?;
            if let Some(existing) = operation(&tx, &receipt.source_run_id, &receipt.source_incarnation)?
                && existing.state.is_committed()
            {
                return Ok(existing);
            }
            if operation(&tx,&receipt.source_run_id,&receipt.source_incarnation)?.is_some() {
                tx.execute("UPDATE lane_handoffs SET request_json=?3,state='preparing',goal_json=NULL,todo_json=NULL WHERE source_run_id=?1 AND source_incarnation=?2",params![receipt.source_run_id,receipt.source_incarnation,receipt.request_json])?;
            } else {
                tx.execute("INSERT INTO lane_handoffs(source_run_id,source_incarnation,target_run_id,lane_id,request_json,state) VALUES(?1,?2,?3,?4,?5,'preparing')",params![receipt.source_run_id,receipt.source_incarnation,receipt.target_run_id,receipt.lane_id,receipt.request_json])?;
            }
            let result=operation(&tx,&receipt.source_run_id,&receipt.source_incarnation)?.expect("reserved receipt");
            tx.commit()?;
            Ok(result)
        })
    }
    pub fn handoff_operation(
        &self,
        source: String,
        incarnation: String,
    ) -> Result<Option<HandoffReceipt>, StorageError> {
        self.lane_job(false, move |conn| operation(conn, &source, &incarnation))
    }
    pub fn handoff_for_source(
        &self,
        source: String,
    ) -> Result<Option<HandoffReceipt>, StorageError> {
        self.lane_job(false,move|conn|Ok(conn.query_row(&format!("SELECT {COLUMNS} FROM lane_handoffs WHERE source_run_id=?1 ORDER BY rowid DESC LIMIT 1"),[source],decode).optional()?))
    }
    pub fn handoff_for_target(
        &self,
        target: String,
    ) -> Result<Option<HandoffReceipt>, StorageError> {
        self.lane_job(false, move |conn| {
            Ok(conn
                .query_row(
                    &format!("SELECT {COLUMNS} FROM lane_handoffs WHERE target_run_id=?1"),
                    [target],
                    decode,
                )
                .optional()?)
        })
    }
    pub fn handoff_question_owner(
        &self,
        id: String,
    ) -> Result<Option<HandoffQuestionOwner>, StorageError> {
        self.lane_job(false, move |conn| question_owner(conn, &id))
    }
    /// Bind only questions that this execution still owns, without reassigning a
    /// previous incarnation's committed question. bind supplies the normal caps
    /// and tombstone fences; partial pre-commit links confer no answer authority.
    pub fn prepare_handoff_questions(
        &self,
        source: String,
        incarnation: String,
    ) -> Result<(), StorageError> {
        self.lane_job(true,move|conn|{
            let receipt=operation(conn,&source,&incarnation)?.ok_or_else(||StorageError::Serialization("handoff receipt missing".into()))?;
            if receipt.state.is_committed(){return Err(StorageError::Serialization("handoff already committed".into()));}
            let mut ids=Vec::new();
            for question in crate::repo::user_questions::for_run(conn,&source)? {
                if question_owner(conn,&question.id)?.is_none_or(|o|!o.committed && o.source_run_id==source){ids.push(question.id);}
            }
            crate::repo::user_questions::bind(conn,&source,&receipt.target_run_id,&ids)?;
            let tx=conn.unchecked_transaction()?;
            for id in ids {
                tx.execute("INSERT INTO lane_handoff_questions(question_id,source_run_id,source_incarnation,target_run_id) VALUES(?1,?2,?3,?4) ON CONFLICT(question_id) DO UPDATE SET source_incarnation=excluded.source_incarnation,target_run_id=excluded.target_run_id WHERE committed=0 AND source_run_id=excluded.source_run_id",params![id,source,incarnation,receipt.target_run_id])?;
            }
            tx.commit()?;
            Ok(())
        })
    }
    /// Record a failed provider admission without replacing the target or seed.
    pub fn note_handoff_start_failure(
        &self,
        target: String,
        reason: String,
    ) -> Result<(), StorageError> {
        self.lane_job(true,move |conn| {
            let reason = secret_guard::SecretRedactor::from_env().redact(&reason).text;
            let reason: String = reason.chars().take(1024).collect();
            conn.execute("UPDATE lane_handoffs SET state='recoverable_start_failure',failure=?2 WHERE target_run_id=?1 AND state IN ('committed','recoverable_start_failure')",params![target,reason])?;
            Ok(())
        })
    }

    /// A trusted, explicit host continuation can resume this committed operation;
    /// startup recovery and automatic wake must never lift its operator stop.
    pub fn resume_handoff_start(&self, target: String) -> Result<(), StorageError> {
        self.lane_job(true,move |conn| {
            conn.execute("UPDATE lanes SET start_blocked_reason=NULL WHERE generation=0 AND lane_id IN (SELECT lane_id FROM lane_handoffs WHERE target_run_id=?1 AND state IN ('committed','recoverable_start_failure'))",[target])?;
            Ok(())
        })
    }

    /// Prepared is not recoverable. Commit is the responsibility transfer boundary.
    pub fn advance_handoff(
        &self,
        source: String,
        incarnation: String,
        expected: HandoffState,
        next: HandoffState,
        goal_json: Option<String>,
        todo_json: Option<String>,
    ) -> Result<HandoffReceipt, StorageError> {
        self.lane_job(true, move |conn| {
            if !matches!((expected,next),(HandoffState::Preparing,HandoffState::Prepared)|(HandoffState::Prepared,HandoffState::Committed)) {
                return Err(StorageError::Serialization("invalid handoff transition".into()));
            }
            let guard=crate::entity::SecretGuard::from_env();
            for (field,text) in [("goal",goal_json.as_deref()),("todo",todo_json.as_deref())] {if let Some(text)=text {guard.check_text("handoff",field,text)?;}}
            let label = |state| match state {HandoffState::Preparing=>"preparing",HandoffState::Prepared=>"prepared",HandoffState::Committed=>"committed",HandoffState::Started=>"started",HandoffState::RecoverableStartFailure=>"recoverable_start_failure"};
            let tx=conn.unchecked_transaction()?;
            let changed = tx.execute("UPDATE lane_handoffs SET state=?4,goal_json=COALESCE(?5,goal_json),todo_json=COALESCE(?6,todo_json) WHERE source_run_id=?1 AND source_incarnation=?2 AND state=?3",params![source,incarnation,label(expected),label(next),goal_json,todo_json])?;
            if changed != 1 { return Err(StorageError::Serialization("stale handoff incarnation or state".into())); }
            if next==HandoffState::Committed {tx.execute("UPDATE lane_handoff_questions SET committed=1 WHERE source_run_id=?1 AND source_incarnation=?2",params![source,incarnation])?;}
            let result=operation(&tx,&source,&incarnation)?.expect("advanced receipt");
            tx.commit()?;
            Ok(result)
        })
    }
}
