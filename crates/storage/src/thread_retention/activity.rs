//! Bounded reader-side activity/goal extraction, independently revisioned from history.
#[cfg(test)]
#[path = "activity_tests.rs"]
mod tests;
use crate::{Database, StorageError, StorageHandle};
use event_bus::{
    AgentRunPhase, EventKind, LifecycleEvent, OrchestratorEvent, ThreadGoalPhase,
    ThreadGoalSnapshot,
};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use std::io::Read;

pub const ACTIVITY_ROWS_PER_TICK: usize = 500;
pub const ACTIVITY_DECODE_BYTES_PER_TICK: usize = 2_097_152;
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivityCursor {
    pub stream: i64,
    pub after_id: i64,
    pub high_water: i64,
    pub complete: bool,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivityReport {
    /// Bounded control-state cursors, not source rows or candidate enumeration.
    pub cursors: Vec<ActivityCursor>,
    pub inspected_rows: usize,
    pub decode_bytes: usize,
    pub skipped: usize,
    pub oversized: usize,
    pub cas_rejected: bool,
    pub backfill_pending: bool,
    pub pending_sources: u64,
    pub legacy_fallback: u64,
    pub missing_activity_and_fallback: u64,
}
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunActivity {
    pub last_actual_activity_at: Option<i64>,
    pub legacy_activity_upper_bound: Option<i64>,
    pub ready: bool,
    pub revision: i64,
    pub failure: Option<String>,
}
#[derive(Debug)]
enum Evidence {
    Activity(String, i64),
    Goal(Box<ThreadGoalSnapshot>, i64, String),
    Delivery(String, String, i64),
    None,
    Failed(String),
}
#[derive(Debug)]
struct Item {
    source: i64,
    id: i64,
    scope: String,
    pending_scope: String,
    pending_revision: Option<i64>,
    evidence: Evidence,
    fallback: Option<i64>,
    legacy: bool,
}
/// A reader snapshot, decoded outside the writer. Fields are private to prevent forged ACKs.
#[derive(Debug)]
pub struct ActivityPage {
    revision: i64,
    items: Vec<Item>,
    cursors: Vec<(i64, i64, i64, bool)>,
    report: ActivityReport,
}
fn invalid() -> StorageError {
    StorageError::Serialization("activity source unavailable".into())
}
fn scope_info(c: &Connection, run: &str) -> Result<(Option<i64>, bool), StorageError> {
    let bounds:(i64,i64)=c.query_row("SELECT legacy_context_rowid,legacy_binding_rowid FROM retention_activity_control WHERE id=1",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    let context: Option<(i64, i64)> = c
        .prepare_cached("SELECT rowid,updated_at_ns FROM run_contexts WHERE run_id=?1")?
        .query_row([run], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let old_binding: bool = c
        .prepare_cached("SELECT EXISTS(SELECT 1 FROM thread_runs WHERE run_id=?1 AND rowid<=?2)")?
        .query_row(params![run, bounds.1], |r| r.get(0))?;
    Ok((
        context.filter(|v| v.0 <= bounds.0).map(|v| v.1),
        old_binding || context.is_some_and(|v| v.0 <= bounds.0),
    ))
}
// Metadata first: never SELECT a page of payloads before checking the byte budget.
fn event(
    c: &Connection,
    id: i64,
    remaining: usize,
    at_head: bool,
    report: &mut ActivityReport,
) -> Result<Option<Evidence>, StorageError> {
    type EventHeader = (String, Option<String>, Option<String>, i64, String, usize);
    let row:Option<EventHeader>=c.prepare_cached("SELECT kind,subkind,run_id,wall_clock_ns,typeof(payload),octet_length(payload) FROM events WHERE id=?1")?.query_row([id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional()?;
    let Some((kind, sub, run, at, ty, stored)) = row else {
        return Ok(Some(Evidence::None));
    };
    if matches!(
        (kind.as_str(), sub.as_deref()),
        (
            "Message",
            Some("MessageCompleted" | "FinalResultPublished" | "MessagePart")
        ) | ("Lifecycle", Some("TurnCompleted"))
    ) {
        return Ok(Some(run.map_or(
            Evidence::Failed("missing run attribution".into()),
            |r| Evidence::Activity(r, at),
        )));
    }
    let prefix_bytes = if ty == "blob" { stored.min(18) } else { 0 };
    if prefix_bytes > remaining {
        return Ok(None);
    }
    let plain = if ty == "blob" {
        let prefix: Vec<u8> = c
            .prepare_cached("SELECT substr(payload,1,18) FROM events WHERE id=?1")?
            .query_row([id], |r| r.get(0))?;
        report.decode_bytes += prefix.len();
        match zstd::zstd_safe::get_frame_content_size(&prefix) {
            Ok(Some(n)) => usize::try_from(n).unwrap_or(usize::MAX),
            _ => usize::MAX,
        }
    } else if ty == "text" {
        stored
    } else {
        usize::MAX
    };
    // Account for the preflight prefix as well as the later full blob read.
    // Expanded length remains the lower bound for compressed sources.
    let bytes = plain.max(stored.saturating_add(prefix_bytes));
    if bytes > remaining {
        if !at_head && bytes <= ACTIVITY_DECODE_BYTES_PER_TICK {
            return Ok(None);
        }
        report.oversized += 1;
        return Ok(Some(Evidence::Failed("decode budget exceeded".into())));
    }
    report.decode_bytes += bytes - prefix_bytes;
    let mut stmt = c.prepare_cached("SELECT payload FROM events WHERE id=?1")?;
    let mut rows = stmt.query([id])?;
    let row = rows.next()?.ok_or_else(invalid)?;
    let decoded = match row.get_ref(0)? {
        rusqlite::types::ValueRef::Text(v) => v.to_vec(),
        rusqlite::types::ValueRef::Blob(v) => {
            let mut data = Vec::with_capacity(plain.min(4096));
            let Ok(decoder) = zstd::stream::read::Decoder::new(v) else {
                return Ok(Some(Evidence::Failed("invalid compressed source".into())));
            };
            if decoder
                .take(plain as u64 + 1)
                .read_to_end(&mut data)
                .is_err()
            {
                return Ok(Some(Evidence::Failed("invalid compressed source".into())));
            }
            if data.len() != plain {
                return Ok(Some(Evidence::Failed("invalid compressed source".into())));
            }
            data
        }
        _ => return Ok(Some(Evidence::Failed("invalid source".into()))),
    };
    // Token-form bodies need not be resolved: only lifecycle, identity and goal metadata
    // are used. Message activity is header-only. No content_blobs reads occur here.
    let Ok(value) = serde_json::from_slice::<EventKind>(&decoded) else {
        return Ok(Some(Evidence::Failed("invalid source".into())));
    };
    Ok(Some(match value {
        EventKind::Message(
            event_bus::MessageEvent::MessageCompleted { run_id, .. }
            | event_bus::MessageEvent::FinalResultPublished { run_id, .. }
            | event_bus::MessageEvent::MessagePart { run_id, .. },
        ) => Evidence::Activity(run_id, at),
        EventKind::Lifecycle(LifecycleEvent::TurnCompleted { run_id, .. }) => {
            Evidence::Activity(run_id, at)
        }
        EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
            run_id, from, to, ..
        }) if from != to && (to == AgentRunPhase::Running || from == AgentRunPhase::Running) => {
            Evidence::Activity(run_id, at)
        }
        EventKind::Orchestrator(OrchestratorEvent::ThreadGoalUpdated { snapshot }) => {
            let related =
                serde_json::to_string(&snapshot.related_root_run_ids).map_err(|_| invalid())?;
            Evidence::Goal(Box::new(snapshot), id, related)
        }
        EventKind::AgentMessage(event_bus::AgentMessageEvent::Delivered { message, .. }) => {
            Evidence::Delivery(message.message_id, message.recipient_run_id, at)
        }
        _ => {
            report.skipped += 1;
            Evidence::None
        }
    }))
}
fn item(
    c: &Connection,
    source: i64,
    id: i64,
    scope: String,
    rev: Option<i64>,
    report: &mut ActivityReport,
) -> Result<Option<Item>, StorageError> {
    let at_head = report.inspected_rows == 0;
    report.inspected_rows += 1;
    let evidence = match source {
        0 => {
            let remaining = ACTIVITY_DECODE_BYTES_PER_TICK - report.decode_bytes;
            let Some(e) = event(c, id, remaining, at_head, report)? else {
                return Ok(None);
            };
            e
        }
        1 | 2 => {
            let table = if source == 1 {
                "host_input_receipts"
            } else {
                "host_chat_submissions"
            };
            c.prepare_cached(&format!(
                "SELECT run_id,accepted_at_ns FROM {table} WHERE rowid=?1"
            ))?
            .query_row([id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .optional()?
            .map_or(Evidence::None, |(run, at)| Evidence::Activity(run, at))
        }
        _ => Evidence::None,
    };
    let pending_scope = scope.clone();
    let scope = match &evidence {
        Evidence::Activity(run, _) | Evidence::Delivery(_, run, _) => run.clone(),
        Evidence::Goal(g, _, _) => g.root_run_id.clone(),
        _ => scope,
    };
    let (fallback, legacy) = scope_info(c, &scope)?;
    Ok(Some(Item {
        source,
        id,
        scope,
        pending_scope,
        pending_revision: rev,
        evidence,
        fallback,
        legacy,
    }))
}
impl Database {
    /// Reader snapshot. At most 500 inspected source rows and 2MiB of decode data.
    pub fn activity_page(&self) -> Result<ActivityPage, StorageError> {
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Deferred)?;
        let revision = tx.query_row(
            "SELECT revision FROM retention_activity_control WHERE id=1",
            [],
            |r| r.get(0),
        )?;
        let mut page = ActivityPage {
            revision,
            items: vec![],
            cursors: vec![],
            report: ActivityReport::default(),
        };
        let initial_work:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM retention_activity_cursors WHERE complete=0 AND after_id<high_water)",[],|r|r.get(0))?;
        let queue_limit = if initial_work {
            250
        } else {
            ACTIVITY_ROWS_PER_TICK
        };
        let queued=tx.prepare("SELECT source,source_id,run_id,revision FROM retention_activity_pending ORDER BY source,source_id,run_id LIMIT ?1")?.query_map([queue_limit],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,i64>(1)?,r.get::<_,String>(2)?,r.get::<_,i64>(3)?)))?.collect::<Result<Vec<_>,_>>()?;
        for (source, id, scope, rev) in queued {
            let Some(i) = item(&tx, source, id, scope, Some(rev), &mut page.report)? else {
                break;
            };
            page.items.push(i);
        }
        let cursors=tx.prepare("SELECT stream,kind,subkind,after_id,high_water FROM retention_activity_cursors WHERE complete=0 ORDER BY stream")?.query_map([],|r|Ok((r.get::<_,i64>(0)?,r.get::<_,String>(1)?,r.get::<_,Option<String>>(2)?,r.get::<_,i64>(3)?,r.get::<_,i64>(4)?)))?.collect::<Result<Vec<_>,_>>()?;
        for (stream, kind, sub, old_after, high_water) in cursors {
            let mut after = old_after;
            let mut complete = false;
            while page.report.inspected_rows < ACTIVITY_ROWS_PER_TICK {
                let source = match kind.as_str() {
                    "host_input_receipts" => 1,
                    "host_chat_submissions" => 2,
                    "thread_runs" => 3,
                    "run_contexts" => 4,
                    _ => 0,
                };
                let next: Option<(i64, String)> = if source == 0 {
                    tx.prepare_cached("SELECT id,coalesce(run_id,'') FROM events INDEXED BY idx_events_kind_subkind_id WHERE kind=?1 AND subkind IS ?2 AND id>?3 AND id<=?4 ORDER BY id LIMIT 1")?.query_row(params![kind,sub,after,high_water],|r|Ok((r.get(0)?,r.get(1)?))).optional()?
                } else {
                    tx.prepare_cached(&format!(
                        "SELECT rowid,run_id FROM {kind} WHERE rowid>?1 AND rowid<=?2 ORDER BY rowid LIMIT 1"
                    ))?
                    .query_row([after,high_water], |r| Ok((r.get(0)?, r.get(1)?)))
                    .optional()?
                };
                let Some((id, scope)) = next else {
                    complete = true;
                    break;
                };
                let Some(i) = item(&tx, source, id, scope, None, &mut page.report)? else {
                    break;
                };
                after = id;
                page.items.push(i);
            }
            page.cursors.push((stream, old_after, after, complete));
            if page.report.inspected_rows == ACTIVITY_ROWS_PER_TICK
                || page.report.decode_bytes == ACTIVITY_DECODE_BYTES_PER_TICK
            {
                break;
            }
        }
        tx.commit()?;
        Ok(page)
    }
    /// Canonical source/applied comparison, never inferred from the newest event ID.
    pub fn run_activity(&self, run: &str) -> Result<RunActivity, StorageError> {
        let row=self.conn.prepare_cached("SELECT r.actual_ns,r.legacy_upper_ns,r.applied,coalesce(s.revision,0),r.failure,r.legacy FROM retention_activity_runs r LEFT JOIN retention_activity_sources s ON s.run_id=r.run_id WHERE r.run_id=?1")?.query_row([run],|r|Ok((r.get::<_,Option<i64>>(0)?,r.get::<_,Option<i64>>(1)?,r.get::<_,i64>(2)?,r.get::<_,i64>(3)?,r.get::<_,Option<String>>(4)?,r.get::<_,bool>(5)?))).optional()?;
        let Some((actual, fallback, applied, revision, failure, legacy)) = row else {
            return Ok(RunActivity::default());
        };
        let backfill: bool = self.conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM retention_activity_cursors WHERE complete=0)",
            [],
            |r| r.get(0),
        )?;
        let pending: bool = self
            .conn
            .prepare_cached(
                "SELECT EXISTS(SELECT 1 FROM retention_activity_pending WHERE run_id IN (?1,'')) OR EXISTS(SELECT 1 FROM retention_activity_failures WHERE run_id IN (?1,''))",
            )?
            .query_row([run], |r| r.get(0))?;
        Ok(RunActivity {
            last_actual_activity_at: actual,
            legacy_activity_upper_bound: fallback,
            ready: !backfill
                && !pending
                && applied == revision
                && failure.is_none()
                && (!legacy || actual.is_some() || fallback.is_some()),
            revision,
            failure,
        })
    }
    pub fn activity_report(&self) -> Result<ActivityReport, StorageError> {
        let mut r = ActivityReport::default();
        progress(&self.conn, &mut r)?;
        Ok(r)
    }
    pub fn activity_goal_protected(&self, thread: &str) -> Result<bool, StorageError> {
        Ok(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM retention_activity_bindings b LEFT JOIN retention_activity_goals g ON g.goal_id=b.goal_id WHERE b.thread_id=?1 AND (b.uncertain=1 OR (b.goal_id IS NOT NULL AND (g.goal_id IS NULL OR g.complete=0 OR g.uncertain=1))))")?.query_row([thread],|r|r.get(0))?)
    }
}
fn goal(
    c: &Connection,
    g: &ThreadGoalSnapshot,
    pos: i64,
    related: &str,
) -> Result<(), StorageError> {
    let prior:Option<(String,String,i64,String)>=c.prepare_cached("SELECT thread_id,root_run_id,event_id,related FROM retention_activity_goals WHERE goal_id=?1")?.query_row([&g.goal_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional()?;
    if let Some((thread, root, oldpos, oldrelated)) = &prior {
        if *oldpos > pos {
            return Ok(());
        }
        let contains = serde_json::from_str::<Vec<String>>(oldrelated)
            .unwrap_or_default()
            .contains(&g.root_run_id);
        if thread != &g.thread_id && contains && !g.related_root_run_ids.contains(root) {
            return Ok(());
        }
        if thread != &g.thread_id {
            c.prepare_cached("UPDATE retention_activity_bindings SET goal_id=NULL,event_id=?3,uncertain=0 WHERE thread_id=?1 AND goal_id=?2 AND event_id<=?3")?.execute(params![thread,g.goal_id,pos])?;
        }
    }
    c.prepare_cached("INSERT INTO retention_activity_goals(goal_id,thread_id,root_run_id,related,complete,event_id) SELECT ?1,?2,?3,?4,?5,?6 WHERE NOT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=?2) AND NOT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?3) ON CONFLICT(goal_id) DO UPDATE SET thread_id=excluded.thread_id,root_run_id=excluded.root_run_id,related=excluded.related,complete=excluded.complete,event_id=excluded.event_id,uncertain=0 WHERE retention_activity_goals.event_id<=excluded.event_id")?.execute(params![g.goal_id,g.thread_id,g.root_run_id,related,g.phase==ThreadGoalPhase::Complete,pos])?;
    c.prepare_cached("INSERT INTO retention_activity_bindings(thread_id,goal_id,event_id) SELECT ?1,?2,?3 WHERE NOT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id=?1) ON CONFLICT(thread_id) DO UPDATE SET goal_id=excluded.goal_id,event_id=excluded.event_id,uncertain=0 WHERE retention_activity_bindings.event_id<=excluded.event_id")?.execute(params![g.thread_id,g.goal_id,pos])?;
    Ok(())
}
impl StorageHandle {
    pub fn apply_activity_page(&self, page: ActivityPage) -> Result<ActivityReport, StorageError> {
        self.activity_job(true,move|c|{
            let tx=Transaction::new_unchecked(c,TransactionBehavior::Immediate)?;
            let revision:i64=tx.query_row("SELECT revision FROM retention_activity_control WHERE id=1",[],|r|r.get(0))?;
            let mut report=page.report;progress(&tx,&mut report)?;
            if revision!=page.revision{report.cas_rejected=true;return Ok(report)}
            for (stream,old,_,_) in &page.cursors {
                let current:i64=tx.query_row("SELECT after_id FROM retention_activity_cursors WHERE stream=?1",[stream],|r|r.get(0))?;
                if current!=*old{report.cas_rejected=true;return Ok(report)}
            }
            for i in page.items {
                let tombstoned:bool=tx.prepare_cached("SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id=?1)")?.query_row([&i.scope],|r|r.get(0))?;
                if !tombstoned {
                    // Capture the legacy upper bound once, only if no activity evidence is
                    // already available when this run is first processed.
                    let fallback=if matches!(i.evidence,Evidence::Activity(..)|Evidence::Delivery(..)){None}else{i.fallback};
                    tx.prepare_cached("INSERT INTO retention_activity_runs(run_id,legacy_upper_ns,captured,legacy) VALUES(?1,?2,1,?3) ON CONFLICT(run_id) DO NOTHING")?.execute(params![i.scope,fallback,i.legacy])?;
                    tx.prepare_cached("DELETE FROM retention_activity_failures WHERE source=?1 AND source_id=?2")?.execute(params![i.source,i.id])?;
                    match i.evidence {
                        Evidence::Activity(run,at)=>{tx.prepare_cached("UPDATE retention_activity_runs SET actual_ns=max(coalesce(actual_ns,?2),?2) WHERE run_id=?1")?.execute(params![run,at])?;},
                        Evidence::Delivery(id,run,at)=>{
                            let inserted=tx.prepare_cached("INSERT OR IGNORE INTO retention_activity_deliveries VALUES(?1,?2,?3)")?.execute(params![id,run,at])?;
                            if inserted>0{tx.prepare_cached("UPDATE retention_activity_runs SET actual_ns=max(coalesce(actual_ns,?2),?2) WHERE run_id=?1")?.execute(params![run,at])?;}
                        },
                        Evidence::Goal(g,pos,related)=>goal(&tx,&g,pos,&related)?,
                        Evidence::Failed(reason)=>{tx.prepare_cached("INSERT INTO retention_activity_failures(source,source_id,run_id,reason) VALUES(?1,?2,?3,?4)")?.execute(params![i.source,i.id,i.scope,reason])?;},
                        Evidence::None=>{if i.source==0 {tx.prepare_cached("UPDATE retention_activity_goals SET uncertain=1 WHERE event_id=?1")?.execute([i.id])?;}},
                    }
                }
                if let Some(rev)=i.pending_revision {tx.prepare_cached("DELETE FROM retention_activity_pending WHERE source=?1 AND source_id=?2 AND revision=?3 AND run_id=?4")?.execute(params![i.source,i.id,rev,i.pending_scope])?;}
                for scope in [&i.scope, &i.pending_scope] {
                    tx.prepare_cached("UPDATE retention_activity_runs SET applied=coalesce((SELECT revision FROM retention_activity_sources WHERE run_id=?1),0),failure=(SELECT reason FROM retention_activity_failures WHERE run_id=?1 LIMIT 1) WHERE run_id=?1 AND NOT EXISTS(SELECT 1 FROM retention_activity_pending WHERE run_id=?1)")?.execute([scope])?;
                }
            }
            for (stream,_,after,complete) in page.cursors {tx.prepare_cached("UPDATE retention_activity_cursors SET after_id=?2,complete=?3 WHERE stream=?1")?.execute(params![stream,after,complete])?;}
            report.backfill_pending=tx.query_row("SELECT EXISTS(SELECT 1 FROM retention_activity_cursors WHERE complete=0)",[],|r|r.get(0))?;
            report.pending_sources=tx.query_row("SELECT count(*) FROM retention_activity_pending",[],|r|r.get(0))?;
            progress(&tx,&mut report)?;tx.commit()?;Ok(report)
        })
    }
}

impl Database {
    /// Direct canonical protection reads, never owner/claim classification.
    pub fn activity_thread_hold(&self, thread: &str) -> Result<Option<&'static str>, StorageError> {
        let unknown_goal:bool=self.conn.query_row("SELECT EXISTS(SELECT 1 FROM retention_activity_pending p JOIN events e ON p.source=0 AND p.source_id=e.id WHERE e.subkind IS NULL OR (e.kind='Orchestrator' AND e.subkind='ThreadGoalUpdated'))",[],|r|r.get(0))?;
        if unknown_goal {
            return Ok(Some("SourceRevisionBehind"));
        }
        if self.activity_goal_protected(thread)? {
            return Ok(Some("UnfinishedGoal"));
        }
        if self
            .conn
            .prepare_cached("SELECT EXISTS(SELECT 1 FROM eval_samples WHERE thread_id=?1)")?
            .query_row([thread], |r| r.get::<_, bool>(0))?
        {
            return Ok(Some("EvaluationSample"));
        }
        let lane:Option<(String,bool,bool)>=self.conn.prepare_cached("SELECT policy,EXISTS(SELECT 1 FROM lane_closes c WHERE c.close_id=l.current_close_id AND c.lane_id=l.lane_id),management_revision>1 AND management_projection_revision<management_revision FROM lanes l WHERE thread_id=?1")?.query_row([thread],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        if let Some((policy, closed, unapplied)) = lane {
            if unapplied {
                return Ok(Some("FamilyUnresolved"));
            }
            if matches!(policy.as_str(), "attached" | "summary") && !closed {
                return Ok(Some("OpenManagedLane"));
            }
        }
        Ok(None)
    }
    pub fn activity_question_protected(&self, run: &str) -> Result<bool, StorageError> {
        Ok(self.conn.prepare_cached("SELECT EXISTS(SELECT 1 FROM user_questions WHERE run_id=?1 AND answered=0) OR EXISTS(SELECT 1 FROM user_questions WHERE root_run_id=?1 AND answered=0) OR EXISTS(SELECT 1 FROM user_question_links l JOIN user_questions q ON q.id=l.question_id WHERE l.run_id=?1 AND q.answered=0)")?.query_row([run],|r|r.get(0))?)
    }
}

impl Database {
    pub fn activity_binding_revision(&self) -> Result<i64, StorageError> {
        Ok(self.conn.query_row(
            "SELECT binding_revision FROM retention_activity_control WHERE id=1",
            [],
            |r| r.get(0),
        )?)
    }
    /// The existing (thread_id,rowid) index supports bounded keyset enumeration.
    pub fn activity_thread_run_page(
        &self,
        thread: &str,
        after: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String)>, StorageError> {
        Ok(self.conn.prepare_cached("SELECT rowid,run_id FROM thread_runs INDEXED BY idx_thread_runs_thread WHERE thread_id=?1 AND rowid>?2 ORDER BY rowid LIMIT ?3")?.query_map(params![thread,after,limit as i64],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<Vec<_>,_>>()?)
    }
}

fn progress(c: &Connection, r: &mut ActivityReport) -> Result<(), StorageError> {
    r.pending_sources =
        c.query_row("SELECT count(*) FROM retention_activity_pending", [], |r| {
            r.get(0)
        })?;
    r.backfill_pending = c.query_row(
        "SELECT EXISTS(SELECT 1 FROM retention_activity_cursors WHERE complete=0)",
        [],
        |r| r.get(0),
    )?;
    (r.legacy_fallback,r.missing_activity_and_fallback)=c.query_row("SELECT count(*) FILTER(WHERE legacy_upper_ns IS NOT NULL),count(*) FILTER(WHERE legacy=1 AND actual_ns IS NULL AND legacy_upper_ns IS NULL) FROM retention_activity_runs",[],|r|Ok((r.get(0)?,r.get(1)?)))?;
    r.cursors=c.prepare_cached("SELECT stream,after_id,complete,high_water FROM retention_activity_cursors ORDER BY stream LIMIT 64")?.query_map([],|r|Ok(ActivityCursor{stream:r.get(0)?,after_id:r.get(1)?,complete:r.get(2)?,high_water:r.get(3)?}))?.collect::<Result<Vec<_>,_>>()?;
    Ok(())
}
