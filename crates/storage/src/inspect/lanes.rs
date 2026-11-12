//! `lane_diagnostics` of `inspect run`: what the lane ledger says about the lanes a run
//! leads, runs in or opened, and about the delivery to its thread. Everything is read
//! in one snapshot, derived from the stored rows with the types that wrote them, and
//! bounded; nothing is written, claimed or re-evaluated. A value that cannot be read
//! is an error, never a zero or an empty list in its place.
//!
//! Text the lanes carry (briefs, summaries, instruction bodies, questions) is not
//! shown: the section is about identities, revisions, counts and states.

use std::collections::BTreeSet;

use event_bus::{LaneStatus, lane::LaneAttentionFilter};
use rusqlite::{Connection, OptionalExtension, params};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::lane_delivery::LaneWakeGuard;
use crate::lanes::{LaneRecord, LaneReport};
use crate::{Database, StorageError};

/// Most lanes, episodes, destinations or attention items listed in each list.
pub const LANE_DIAGNOSTICS_LIST_MAX: usize = 20;
/// Most stall or recovery episodes shown per lane or destination (newest first).
const EPISODES_SHOWN: usize = 5;
/// Most stored uncollected lane IDs whose ledger state is evaluated.
pub const LANE_DIAGNOSTICS_UNCOLLECTED_EVALUATED_MAX: usize = 200;

impl Database {
    /// The lane section of `inspect run`: scope, lanes, delivery state of the run's
    /// thread, and the current ledger state of `uncollected_lane_ids` (the cohort a
    /// stored run snapshot names). `Value::Null` for a deleted run.
    ///
    /// # Errors
    /// Returns an error if SQLite access fails or a stored value cannot be decoded.
    pub fn inspect_lane_diagnostics(
        &self,
        run_id: &str,
        uncollected_lane_ids: &[String],
    ) -> Result<Value, StorageError> {
        crate::repo::run_messages::read_snapshot(&self.conn, || {
            diagnostics(&self.conn, run_id, uncollected_lane_ids)
        })
    }
}

fn decode<T: DeserializeOwned>(text: &str, what: &str) -> Result<T, StorageError> {
    serde_json::from_str(text)
        .map_err(|error| StorageError::Serialization(format!("stored {what}: {error}")))
}

fn to_value(value: &impl serde::Serialize) -> Result<Value, StorageError> {
    serde_json::to_value(value).map_err(|error| StorageError::Serialization(error.to_string()))
}

struct Scope {
    thread_id: Option<String>,
    /// The root run the thread's row names. Unknown when the thread has no row.
    thread_root_run_id: Option<String>,
    /// The run is the root the thread runs now, so what the thread manages now is the
    /// run's own. Never assumed: it needs the thread's row to name this run.
    current_root: bool,
}

fn scope(conn: &Connection, run_id: &str) -> Result<Scope, StorageError> {
    let thread_id: Option<String> = conn
        .query_row(
            "SELECT thread_id FROM thread_runs WHERE run_id = ?1
             AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones)",
            [run_id],
            |row| row.get(0),
        )
        .optional()?;
    let thread_root_run_id: Option<String> = match &thread_id {
        Some(thread) => conn
            .query_row(
                "SELECT root_run_id FROM threads WHERE thread_id = ?1",
                [thread],
                |row| row.get(0),
            )
            .optional()?
            .flatten(),
        None => None,
    };
    Ok(Scope {
        current_root: thread_root_run_id.as_deref() == Some(run_id),
        thread_id,
        thread_root_run_id,
    })
}

fn diagnostics(
    conn: &Connection,
    run_id: &str,
    uncollected: &[String],
) -> Result<Value, StorageError> {
    let hidden: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM run_tombstones WHERE run_id = ?1)",
        [run_id],
        |row| row.get(0),
    )?;
    if hidden {
        return Ok(Value::Null);
    }
    let scope = scope(conn, run_id)?;
    let lead_thread = scope
        .thread_id
        .as_deref()
        .filter(|_| scope.current_root)
        .map(str::to_owned);
    let in_thread = scope.thread_id.clone();
    let filter = "(thread_id = ?1 OR lead_thread_id = ?2 OR origin_run_id = ?3) \
                  AND thread_id NOT IN (SELECT thread_id FROM thread_tombstones)";
    let total: usize = conn.query_row(
        &format!("SELECT COUNT(*) FROM lanes WHERE {filter}"),
        params![in_thread, lead_thread, run_id],
        |row| row.get(0),
    )?;
    let ids: Vec<String> = conn
        .prepare(&format!(
            "SELECT lane_id FROM lanes WHERE {filter}
             ORDER BY last_activity_at_ns DESC, lane_id LIMIT ?4"
        ))?
        .query_map(
            params![
                in_thread,
                lead_thread,
                run_id,
                i64::try_from(LANE_DIAGNOSTICS_LIST_MAX).unwrap_or(i64::MAX)
            ],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let mut attention = Attention::default();
    let mut lanes = Vec::new();
    let mut lane_thread_leads = BTreeSet::new();
    for id in &ids {
        let Some(lane) = crate::repo::lanes::get(conn, id)? else {
            continue;
        };
        let mut relations = Vec::new();
        if in_thread.as_deref() == Some(lane.thread_id.as_str()) {
            relations.push(if lane.root_run_id.as_deref() == Some(run_id) {
                "lane_root_run"
            } else {
                "lane_thread_run"
            });
            if let Some(lead) = &lane.lead_thread_id {
                lane_thread_leads.insert(lead.clone());
            }
        }
        if lead_thread.is_some() && lane.lead_thread_id == lead_thread {
            relations.push("managed_by_this_run");
        }
        if lane.origin_run_id == run_id {
            relations.push("opened_by_this_run");
        }
        lanes.push(lane_json(conn, &lane, &relations, &mut attention)?);
    }
    let mut destinations = Vec::new();
    let in_lane_root = lanes.iter().any(|lane| {
        lane["relations"]
            .as_array()
            .is_some_and(|relations| relations.iter().any(|r| r == "lane_root_run"))
    });
    let mut destination_ids = BTreeSet::new();
    if scope.current_root || in_lane_root {
        destination_ids.extend(in_thread.clone());
    }
    destination_ids.extend(lane_thread_leads);
    for destination in destination_ids.iter().take(LANE_DIAGNOSTICS_LIST_MAX) {
        destinations.push(destination_json(conn, destination, run_id)?);
    }
    Ok(json!({
        "run_id": run_id,
        "scope": {
            "thread_id": scope.thread_id,
            "thread_root_run_id": scope.thread_root_run_id,
            "run_is_current_root_of_thread": scope.current_root,
            "managed_lanes_attributed": lead_thread.is_some(),
            "note": "managed_by_this_run and delivery of the thread describe the ledger now, and only for the thread's current root run",
        },
        "lanes": {
            "total": total,
            "shown": lanes.len(),
            "truncated": total > lanes.len(),
            "items": lanes,
        },
        "delivery": {
            "destinations": destinations,
        },
        "uncollected_current": uncollected_json(conn, lead_thread.as_deref(), uncollected)?,
    }))
}

#[derive(Default)]
struct Attention {
    by_lead: std::collections::BTreeMap<String, Vec<event_bus::lane::LaneAttentionItem>>,
}

impl Attention {
    fn for_lane(
        &mut self,
        conn: &Connection,
        lane: &LaneRecord,
    ) -> Result<Vec<Value>, StorageError> {
        let Some(lead) = lane
            .lead_thread_id
            .as_ref()
            .filter(|_| lane.policy.is_managed())
        else {
            return Ok(Vec::new());
        };
        if !self.by_lead.contains_key(lead) {
            let items = crate::repo::lane_control::items(
                conn,
                &LaneAttentionFilter {
                    lead_thread_id: lead.clone(),
                    project_id: None,
                },
            )?;
            self.by_lead.insert(lead.clone(), items);
        }
        self.by_lead[lead]
            .iter()
            .filter(|item| item.lane_id == lane.lane_id)
            .take(LANE_DIAGNOSTICS_LIST_MAX)
            .map(|item| {
                Ok(json!({
                    "id": item.id,
                    "kind": to_value(&item.kind)?,
                    "since_at_ns": item.since_at_ns,
                    "revision": item.revision,
                    "management_revision": item.management_revision,
                    "available_actions": to_value(&item.available_actions)?,
                }))
            })
            .collect()
    }
}

fn report_json(lane: &LaneRecord, report: &LaneReport) -> Value {
    json!({
        "report_seq": report.report_seq,
        "status": report.status.as_str(),
        "producer": report.producer.as_str(),
        "needs_human": report.needs_human,
        "reason_code": report.reason_code,
        "failure_episode": report.failure_episode,
        "has_artifact_revision": report.artifact_revision.is_some(),
        "management_revision": report.management_revision,
        "brief_revision": report.brief_revision,
        "created_at_ns": report.created_at_ns,
        "current_for_management_and_brief": report.management_revision == lane.management_revision
            && report.brief_revision == lane.brief_revision,
    })
}

fn lane_json(
    conn: &Connection,
    lane: &LaneRecord,
    relations: &[&str],
    attention: &mut Attention,
) -> Result<Value, StorageError> {
    let latest = crate::repo::lanes::reports(
        conn,
        &lane.lane_id,
        lane.latest_report_seq.saturating_sub(1),
        1,
    )?
    .into_iter()
    .next();
    let (request_revision, progress_epoch, close_id): (u64, u64, Option<String>) = conn.query_row(
        "SELECT request_revision, progress_epoch, current_close_id FROM lanes WHERE lane_id = ?1",
        [&lane.lane_id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let guard_text: String = conn.query_row(
        "SELECT wake_guard_json FROM lanes WHERE lane_id = ?1",
        [&lane.lane_id],
        |row| row.get(0),
    )?;
    let guard: LaneWakeGuard = decode(&guard_text, "wake guard")?;
    let ready = latest.as_ref().filter(|report| {
        report.status == LaneStatus::Ready && report.management_revision == lane.management_revision
    });
    Ok(json!({
        "lane_id": lane.lane_id,
        "thread_id": lane.thread_id,
        "relations": relations,
        "policy": to_value(&lane.policy)?,
        "lead_thread_id": lane.lead_thread_id,
        "origin_thread_id": lane.origin_thread_id,
        "origin_run_id": lane.origin_run_id,
        "management_revision": lane.management_revision,
        "brief_revision": lane.brief_revision,
        "request_revision": request_revision,
        "progress_epoch": progress_epoch,
        "generation": lane.generation,
        "root_run_id": lane.root_run_id,
        "start_blocked_reason": lane.start_blocked_reason,
        "last_activity_at_ns": lane.last_activity_at_ns,
        "latest_report": latest.as_ref().map(|report| report_json(lane, report)),
        "ready": ready.map(|report| json!({
            "ready_seq": report.report_seq,
            "report_brief_revision": report.brief_revision,
            "brief_revision": lane.brief_revision,
            "management_revision": lane.management_revision,
        })),
        "close": close_json(conn, lane, close_id.as_deref())?,
        "instructions": instructions_json(conn, lane)?,
        "wake_guard": to_value(&guard)?,
        "stall": stall_json(conn, &lane.lane_id)?,
        "attention": attention.for_lane(conn, lane)?,
    }))
}

fn close_json(
    conn: &Connection,
    lane: &LaneRecord,
    close_id: Option<&str>,
) -> Result<Value, StorageError> {
    let Some(close_id) = close_id else {
        return Ok(Value::Null);
    };
    let payload: String = conn.query_row(
        "SELECT payload FROM lane_closes WHERE close_id = ?1",
        [close_id],
        |row| row.get(0),
    )?;
    let close: event_bus::lane::LaneCloseResult = decode(&payload, "lane close")?;
    Ok(json!({
        "close_id": close.close_id,
        "kind": close.kind.as_str(),
        "ready_seq": close.ready_seq,
        "closed_revision": close.closed_revision,
        "management_revision": close.management_revision,
        "brief_revision": close.brief_revision,
        "at_ns": close.at_ns,
        "stands_for_current_management": close.management_revision == lane.management_revision,
    }))
}

fn instructions_json(conn: &Connection, lane: &LaneRecord) -> Result<Value, StorageError> {
    let count: u64 = conn.query_row(
        "SELECT COUNT(*) FROM lane_instructions WHERE lane_id = ?1",
        [&lane.lane_id],
        |row| row.get(0),
    )?;
    let latest: Option<(u64, String, u64, u64, u64)> = conn
        .query_row(
            "SELECT instruction_seq, kind, accepted_management_revision, brief_revision_before,
                    brief_revision_after
             FROM lane_instructions WHERE lane_id = ?1 ORDER BY instruction_seq DESC LIMIT 1",
            [&lane.lane_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let latest = match latest {
        Some((seq, kind, management, before, after)) => {
            let delivery = crate::repo::lane_instructions::delivery(conn, &lane.lane_id, seq)?;
            json!({
                "instruction_seq": seq,
                "kind": kind,
                "accepted_management_revision": management,
                "brief_revision_before": before,
                "brief_revision_after": after,
                "delivery": delivery.map(|delivery| to_value(&delivery)).transpose()?,
            })
        }
        None => Value::Null,
    };
    Ok(json!({ "count": count, "latest": latest }))
}

fn stall_json(conn: &Connection, lane_id: &str) -> Result<Value, StorageError> {
    let total: u64 = conn.query_row(
        "SELECT COUNT(*) FROM lane_stall_episodes WHERE lane_id = ?1",
        [lane_id],
        |row| row.get(0),
    )?;
    let payloads: Vec<String> = conn
        .prepare(
            "SELECT payload FROM lane_stall_episodes WHERE lane_id = ?1
             ORDER BY progress_epoch DESC, management_revision DESC, episode_id LIMIT ?2",
        )?
        .query_map(
            params![lane_id, i64::try_from(EPISODES_SHOWN).unwrap_or(i64::MAX)],
            |row| row.get(0),
        )?
        .collect::<Result<_, _>>()?;
    let mut episodes = Vec::new();
    for payload in payloads {
        let episode: event_bus::lane::LaneStallEpisode = decode(&payload, "stall episode")?;
        episodes.push(to_value(&episode)?);
    }
    Ok(json!({
        "total": total,
        "shown": episodes.len(),
        "truncated": total > u64::try_from(episodes.len()).unwrap_or(u64::MAX),
        "episodes": episodes,
    }))
}

fn destination_json(
    conn: &Connection,
    thread_id: &str,
    run_id: &str,
) -> Result<Value, StorageError> {
    let tombstoned: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM thread_tombstones WHERE thread_id = ?1)",
        [thread_id],
        |row| row.get(0),
    )?;
    if tombstoned {
        return Ok(Value::Null);
    }
    let destination = crate::repo::lane_delivery::destination(conn, thread_id)?;
    let last_nack: Option<String> = conn
        .query_row(
            "SELECT last_nack FROM lane_delivery_destinations WHERE thread_id = ?1",
            [thread_id],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let pending: Vec<(String, u64)> = conn
        .prepare(
            "SELECT event_kind, COUNT(*) FROM lane_notifications
             WHERE destination_thread_id = ?1 AND state = 'pending'
             GROUP BY event_kind ORDER BY event_kind",
        )?
        .query_map([thread_id], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let recovery_total: u64 = conn.query_row(
        "SELECT COUNT(*) FROM lane_recovery_episodes WHERE destination_thread_id = ?1",
        [thread_id],
        |row| row.get(0),
    )?;
    let recoveries =
        crate::repo::lane_delivery::recent_recoveries(conn, thread_id, EPISODES_SHOWN)?;
    let mut episodes = Vec::new();
    for episode in &recoveries {
        let mut value = to_value(episode)?;
        value["saved_root_run_is_this_run"] = json!(episode.saved_root_run_id == run_id);
        episodes.push(value);
    }
    let active = recoveries
        .iter()
        .find(|episode| episode.state != crate::lane_delivery::LaneRecoveryState::Resolved)
        .map(|episode| episode.episode_id.clone());
    Ok(json!({
        "thread_id": thread_id,
        "destination": destination.as_ref().map(to_value).transpose()?,
        "last_nack": last_nack.map(|nack| serde_json::from_str::<Value>(&nack).unwrap_or(Value::String(nack))),
        "owner_is_this_run": destination
            .as_ref()
            .and_then(|d| d.owner.as_ref())
            .is_some_and(|owner| owner.root_run_id == run_id),
        "pending_notifications": pending
            .iter()
            .map(|(kind, count)| json!({"event_kind": kind, "count": count}))
            .collect::<Vec<_>>(),
        "recovery": {
            "total": recovery_total,
            "shown": episodes.len(),
            "truncated": recovery_total > u64::try_from(episodes.len()).unwrap_or(u64::MAX),
            "active_episode_id": active,
            "episodes": episodes,
        },
    }))
}

/// The ledger's state now for a cohort a run snapshot names: whether each lane is still
/// managed by the lead and whether its latest report is a terminal one for the
/// current management and brief (what lifts the creation suppression once reached).
fn uncollected_json(
    conn: &Connection,
    lead_thread: Option<&str>,
    ids: &[String],
) -> Result<Value, StorageError> {
    let Some(lead) = lead_thread else {
        return Ok(json!({
            "evaluated": false,
            "reason": "the run is not the current root of a thread, so no lead's lanes are evaluated for it",
            "stored_lane_ids": ids.len(),
        }));
    };
    let mut lanes = Vec::new();
    let mut managed = 0_usize;
    let mut terminal = 0_usize;
    let evaluated = &ids[..ids.len().min(LANE_DIAGNOSTICS_UNCOLLECTED_EVALUATED_MAX)];
    for id in evaluated {
        let lane = crate::repo::lanes::get(conn, id)?;
        let Some(lane) = lane else {
            if lanes.len() < LANE_DIAGNOSTICS_LIST_MAX {
                lanes.push(json!({"lane_id": id, "exists": false}));
            }
            continue;
        };
        let is_managed = lane.lead_thread_id.as_deref() == Some(lead);
        let latest =
            crate::repo::lanes::reports(conn, id, lane.latest_report_seq.saturating_sub(1), 1)?
                .into_iter()
                .next();
        let is_terminal = is_managed
            && latest.as_ref().is_some_and(|report| {
                matches!(report.status, LaneStatus::Ready | LaneStatus::Failed)
                    && report.management_revision == lane.management_revision
                    && report.brief_revision == lane.brief_revision
            });
        managed += usize::from(is_managed);
        terminal += usize::from(is_terminal);
        if lanes.len() < LANE_DIAGNOSTICS_LIST_MAX {
            lanes.push(json!({
                "lane_id": id,
                "exists": true,
                "managed_by_lead": is_managed,
                "latest_report_status": latest.as_ref().map(|report| report.status.as_str()),
                "terminal_for_current_management": is_terminal,
            }));
        }
    }
    Ok(json!({
        "evaluated": true,
        "lead_thread_id": lead,
        "stored_lane_ids": ids.len(),
        "evaluated_lane_ids": evaluated.len(),
        "managed_count": managed,
        "terminal_count": terminal,
        "lanes_shown": lanes.len(),
        "lanes": lanes,
    }))
}
