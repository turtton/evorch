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
use storage::Database;
use storage::inspect::LANE_DIAGNOSTICS_LIST_MAX;

/// Adds `lane_diagnostics` to the `inspect run` document `result`.
pub(super) fn attach(db: &Database, run_id: &str, result: &mut Value) -> Result<(), String> {
    let stored = match result.get("context").filter(|context| !context.is_null()) {
        Some(context) => {
            stored_uncollected_lanes(&context["config_json"]).map_err(|error| error.to_string())
        }
        None => Err("the run has no stored snapshot".to_owned()),
    };
    let ids: &[String] = stored.as_ref().map_or(&[], |marker| &marker.lane_ids);
    let mut section = db
        .inspect_lane_diagnostics(run_id, ids)
        .map_err(|error| error.to_string())?;
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

fn uncollected(stored: &Result<StoredUncollectedLanes, String>, current: Value) -> Value {
    match stored {
        Err(reason) => json!({
            "stored": {"available": false, "reason": reason},
            "current": current,
            "stored_marker_suppresses_creation_now": Value::Null,
        }),
        Ok(marker) => {
            let terminal = current["terminal_count"].as_u64();
            // The snapshot does not record an injected threshold, so this is the
            // default threshold's verdict.
            let suppresses = terminal.map(|terminal| {
                marker.uncollected_lane_open_count >= marker.default_threshold && terminal > 0
            });
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
/// restore-retry-suppression event to the recovery episode in the ledger.
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
    let mut related = Vec::new();
    let mut uncollected_events = 0_usize;
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
                uncollected_events += 1;
                let marker = stored.as_ref().ok();
                json!({
                    "stored_cohort_id": marker.and_then(|m| m.cohort_id.clone()),
                    "matches_stored_cohort": marker
                        .and_then(|m| m.cohort_id.as_deref())
                        .map(|cohort| Some(cohort) == identity),
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
        related.push(json!({
            "event_id": event["id"],
            "wall_clock_ns": event["wall_clock_ns"],
            "code": code,
            "identity": identity,
            "action": fields.get("action"),
            "correlation": correlation,
        }));
    }
    let marker_reported = stored.as_ref().ok().map(|m| m.suppression_reported);
    json!({
        "events": related,
        "stored_marker_reported_without_event": marker_reported
            .map(|reported| reported && uncollected_events == 0),
    })
}

#[cfg(test)]
mod tests;
