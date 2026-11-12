use super::*;

fn marker(cohort: &str, reported: bool, count: u32) -> StoredUncollectedLanes {
    StoredUncollectedLanes {
        cohort_id: Some(cohort.into()),
        cohort_id_derived: false,
        uncollected_lane_open_count: count,
        lane_ids: vec!["lane-a".into(), "lane-b".into()],
        suppression_reported: reported,
        default_threshold: 100,
    }
}

fn diagnostic(id: i64, code: &str, detail: &str) -> Value {
    json!({
        "id": id,
        "wall_clock_ns": id * 10,
        "kind": "Diagnostic",
        "payload": {"payload": {"code": code, "detail": detail, "source": "test"}},
    })
}

fn creation_event(cohort: &str) -> Value {
    diagnostic(
        1,
        LANE_UNCOLLECTED_CREATION_SUPPRESSED,
        &format!("action=suppress_lane_open\nidentity={cohort}\ncohort={cohort}\nthreshold=100"),
    )
}

fn section_with_episode(episode_id: &str) -> Value {
    json!({"delivery": {"destinations": [{"recovery": {"episodes": [{
        "episode_id": episode_id,
        "state": "suppressed",
        "attempts": 3,
        "revision": 4,
        "failure_report_ref": "lane-report:1",
        "saved_root_run_is_this_run": true,
    }]}}]}})
}

#[test]
fn a_creation_event_is_matched_to_the_cohort_the_snapshot_stores() {
    let stored = Ok(marker("cohort-1", true, 100));
    let related = related_diagnostics(
        &[creation_event("cohort-1"), creation_event("cohort-2")],
        &stored,
        &json!({}),
    );

    assert_eq!(
        related["events"][0]["correlation"]["matches_stored_cohort"],
        true
    );
    assert_eq!(
        related["events"][1]["correlation"]["matches_stored_cohort"],
        false
    );
    assert_eq!(related["events"][0]["identity"], "cohort-1");
    assert_eq!(related["events"][0]["action"], "suppress_lane_open");
    assert_eq!(related["stored_marker_reported_without_event"], false);
}

#[test]
fn a_reported_marker_without_any_event_is_called_out() {
    let stored = Ok(marker("cohort-1", true, 100));

    let related = related_diagnostics(&[], &stored, &json!({}));

    assert_eq!(related["events"], json!([]));
    assert_eq!(related["stored_marker_reported_without_event"], true);
    let unreported = related_diagnostics(&[], &Ok(marker("cohort-1", false, 3)), &json!({}));
    assert_eq!(unreported["stored_marker_reported_without_event"], false);
}

#[test]
fn a_restore_event_is_matched_to_the_recovery_episode_in_the_ledger() {
    let event = diagnostic(
        2,
        LANE_RESTORE_RETRY_SUPPRESSED,
        "action=suppress_restore_retry\nidentity=episode-9\nepisode=episode-9\nattempts=3",
    );
    let missing = diagnostic(
        3,
        LANE_RESTORE_RETRY_SUPPRESSED,
        "action=suppress_restore_retry\nidentity=episode-old",
    );
    let stored = Err("no snapshot".to_owned());

    let related = related_diagnostics(
        &[event, missing],
        &stored,
        &section_with_episode("episode-9"),
    );

    let found = &related["events"][0]["correlation"];
    assert_eq!(found["found"], true);
    assert_eq!(found["episode_in_ledger"]["state"], "suppressed");
    assert_eq!(found["episode_in_ledger"]["attempts"], 3);
    assert_eq!(related["events"][1]["correlation"]["found"], false);
    assert_eq!(related["stored_marker_reported_without_event"], Value::Null);
}

#[test]
fn other_diagnostics_are_not_lane_events() {
    let other = diagnostic(5, "NoProgress", "identity=x");

    let related = related_diagnostics(&[other], &Ok(marker("c", false, 0)), &json!({}));

    assert_eq!(related["events"], json!([]));
}

#[test]
fn an_unreadable_stored_marker_is_reported_as_unavailable_never_as_zero() {
    let value = uncollected(&Err("the run has no stored snapshot".to_owned()), json!({}));

    assert_eq!(value["stored"]["available"], false);
    assert_eq!(value["stored"]["reason"], "the run has no stored snapshot");
    assert_eq!(value["stored"].get("uncollected_lane_open_count"), None);
    assert_eq!(value["stored_marker_suppresses_creation_now"], Value::Null);
}

#[test]
fn the_stored_marker_suppresses_only_while_a_terminal_lane_remains_and_the_count_is_reached() {
    let at_threshold = marker("c", true, 100);
    let with_terminal = json!({"evaluated": true, "terminal_count": 1});
    let collected = json!({"evaluated": true, "terminal_count": 0});

    let suppressing = uncollected(&Ok(at_threshold.clone()), with_terminal.clone());
    let lifted = uncollected(&Ok(at_threshold), collected);
    let below = uncollected(&Ok(marker("c", false, 99)), with_terminal);
    let not_evaluated = uncollected(
        &Ok(marker("c", false, 100)),
        json!({"evaluated": false, "reason": "not current root"}),
    );

    assert_eq!(suppressing["stored_marker_suppresses_creation_now"], true);
    assert_eq!(lifted["stored_marker_suppresses_creation_now"], false);
    assert_eq!(below["stored_marker_suppresses_creation_now"], false);
    assert_eq!(
        not_evaluated["stored_marker_suppresses_creation_now"],
        Value::Null
    );
    assert_eq!(suppressing["stored"]["cohort_id"], "c");
    assert_eq!(suppressing["stored"]["lane_ids_total"], 2);
}
