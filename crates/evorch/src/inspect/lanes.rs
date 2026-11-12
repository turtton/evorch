//! The `lane_diagnostics` section of `inspect run`, assembled from three sources that
//! must not be mixed up: the lane ledger now (storage, one read snapshot), the marker the
//! run's stored snapshot carries (decoded by the runtime's own type, see
//! [`runtime::budget_tracker::stored_uncollected_lanes`]) and the diagnostic events
//! already persisted for the run. Each stays labelled as what it is, and one that cannot
//! be read is reported as such, never as zero.

use std::collections::BTreeMap;

use event_bus::event::diagnostic_codes::{
    LANE_RESTORE_RETRY_SUPPRESSED, LANE_UNCOLLECTED_CREATION_SUPPRESSED,
};
use runtime::budget_tracker::{StoredUncollectedLanes, stored_uncollected_lanes};
use serde_json::{Value, json};
use storage::inspect::LANE_DIAGNOSTICS_LIST_MAX;
use storage::{Database, StorageError};

/// Adds `lane_diagnostics` to the `inspect run` document `result`. Call it inside the
/// database's read snapshot, with the run document read in the same one, so the stored
/// marker, the events and the ledger describe one moment.
pub(super) fn attach(db: &Database, run_id: &str, result: &mut Value) -> Result<(), StorageError> {
    let stored = match result.get("context").filter(|context| !context.is_null()) {
        Some(context) => {
            stored_uncollected_lanes(&context["config_json"]).map_err(|error| error.to_string())
        }
        None => Err("the run has no stored snapshot".to_owned()),
    };
    let ids: &[String] = stored.as_ref().map_or(&[], |marker| &marker.lane_ids);
    let mut section = db.inspect_lane_diagnostics(run_id, ids)?;
    if section.is_null() {
        result["lane_diagnostics"] = Value::Null;
        return Ok(());
    }
    let current = section["uncollected_current"].take();
    let events = diagnostics_of(result);
    section["uncollected"] = uncollected(&stored, current);
    section["related_diagnostics"] = related_diagnostics(&events, &stored, &section);
    if let Some(object) = section.as_object_mut() {
        object.remove("uncollected_current");
    }
    result["lane_diagnostics"] = section;
    Ok(())
}

fn diagnostics_of(result: &Value) -> Vec<Value> {
    result["diagnostics"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

/// Whether the stored marker still suppresses `lane_open`, the way the runtime decides
/// it: the creation count reached the threshold and some lane of the cohort is a
/// terminal one for its current management. `None` when the evidence cannot settle it:
/// no terminal lane was found among the lanes that were evaluated, but some were not.
fn suppresses_creation_now(marker: &StoredUncollectedLanes, current: &Value) -> Option<bool> {
    if marker.uncollected_lane_open_count < marker.default_threshold {
        return Some(false);
    }
    if current["evaluated"] != true {
        return None;
    }
    if current["terminal_count"].as_u64()? > 0 {
        return Some(true);
    }
    current["evaluation_complete"].as_bool()?.then_some(false)
}

fn uncollected(stored: &Result<StoredUncollectedLanes, String>, current: Value) -> Value {
    match stored {
        Err(reason) => json!({
            "stored": {"available": false, "reason": reason},
            "current": current,
            "stored_marker_suppresses_creation_now": Value::Null,
        }),
        Ok(marker) => {
            // The snapshot does not record an injected threshold, so this is the
            // default threshold's verdict.
            let suppresses = suppresses_creation_now(marker, &current);
            json!({
                "stored": {
                    "available": true,
                    "cohort_id": marker.cohort_id,
                    "cohort_id_derived": marker.cohort_id_derived,
                    "uncollected_lane_open_count": marker.uncollected_lane_open_count,
                    "lane_ids_total": marker.lane_ids.len(),
                    "lane_ids": marker.lane_ids.iter().take(LANE_DIAGNOSTICS_LIST_MAX).collect::<Vec<_>>(),
                    "lane_ids_truncated": marker.lane_ids.len() > LANE_DIAGNOSTICS_LIST_MAX,
                    "suppression_reported": marker.suppression_reported,
                    "default_threshold": marker.default_threshold,
                },
                "current": current,
                "stored_marker_suppresses_creation_now": suppresses,
            })
        }
    }
}

/// The `key=value` lines of a lane diagnostic's detail.
fn detail_fields(detail: &str) -> BTreeMap<&str, &str> {
    detail
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect()
}

/// Links the lane diagnostics persisted for the run to the state they describe: a
/// creation-suppression event to the cohort marker the snapshot stores, a
/// restore-retry-suppression event to the recovery episode in the ledger. At most
/// [`LANE_DIAGNOSTICS_LIST_MAX`] events are listed, with the total.
fn related_diagnostics(
    events: &[Value],
    stored: &Result<StoredUncollectedLanes, String>,
    section: &Value,
) -> Value {
    let episodes: Vec<&Value> = section["delivery"]["destinations"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|destination| {
            destination["recovery"]["episodes"]
                .as_array()
                .into_iter()
                .flatten()
        })
        .collect();
    let stored_cohort = stored
        .as_ref()
        .ok()
        .and_then(|marker| marker.cohort_id.as_deref());
    let mut related = Vec::new();
    let mut total = 0_usize;
    let mut event_for_stored_cohort = false;
    for event in events {
        let payload = &event["payload"]["payload"];
        let (Some(code), Some(detail)) = (payload["code"].as_str(), payload["detail"].as_str())
        else {
            continue;
        };
        let fields = detail_fields(detail);
        let identity = fields.get("identity").copied();
        let correlation = match code {
            LANE_UNCOLLECTED_CREATION_SUPPRESSED => {
                event_for_stored_cohort |= stored_cohort.is_some() && stored_cohort == identity;
                let marker = stored.as_ref().ok();
                json!({
                    "stored_cohort_id": stored_cohort,
                    "matches_stored_cohort": stored_cohort.map(|cohort| Some(cohort) == identity),
                    "stored_suppression_reported": marker.map(|m| m.suppression_reported),
                })
            }
            LANE_RESTORE_RETRY_SUPPRESSED => {
                let episode = episodes
                    .iter()
                    .find(|episode| episode["episode_id"].as_str() == identity);
                json!({
                    "episode_in_ledger": episode.map(|episode| json!({
                        "state": episode["state"],
                        "attempts": episode["attempts"],
                        "revision": episode["revision"],
                        "failure_report_ref": episode["failure_report_ref"],
                        "saved_root_run_is_this_run": episode["saved_root_run_is_this_run"],
                    })),
                    "found": episode.is_some(),
                    "note": "an episode older than the newest listed is not matched",
                })
            }
            _ => continue,
        };
        total += 1;
        if related.len() < LANE_DIAGNOSTICS_LIST_MAX {
            related.push(json!({
                "event_id": event["id"],
                "wall_clock_ns": event["wall_clock_ns"],
                "code": code,
                "identity": identity,
                "action": fields.get("action"),
                "correlation": correlation,
            }));
        }
    }
    let marker_reported = stored.as_ref().ok().map(|m| m.suppression_reported);
    json!({
        "total": total,
        "shown": related.len(),
        "truncated": total > related.len(),
        "events": related,
        "stored_marker_reported_without_event_for_its_cohort": marker_reported
            .map(|reported| reported && !event_for_stored_cohort),
    })
}

#[cfg(test)]
mod tests;
