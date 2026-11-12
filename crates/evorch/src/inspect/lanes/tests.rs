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
    assert_eq!(
        related["stored_marker_reported_without_event_for_its_cohort"],
        false
    );
}

#[test]
fn a_reported_marker_without_any_event_is_called_out() {
    let stored = Ok(marker("cohort-1", true, 100));

    let related = related_diagnostics(&[], &stored, &json!({}));

    assert_eq!(related["events"], json!([]));
    assert_eq!(
        related["stored_marker_reported_without_event_for_its_cohort"],
        true
    );
    let unreported = related_diagnostics(&[], &Ok(marker("cohort-1", false, 3)), &json!({}));
    assert_eq!(
        unreported["stored_marker_reported_without_event_for_its_cohort"],
        false
    );
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
    assert_eq!(
        related["stored_marker_reported_without_event_for_its_cohort"],
        Value::Null
    );
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
    let collected = json!({"evaluated": true, "terminal_count": 0, "evaluation_complete": true});

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

#[test]
fn an_event_of_another_cohort_does_not_account_for_the_stored_markers_report() {
    let stored = Ok(marker("cohort-now", true, 100));

    let related = related_diagnostics(&[creation_event("cohort-before")], &stored, &json!({}));

    assert_eq!(
        related["events"][0]["correlation"]["matches_stored_cohort"],
        false
    );
    assert_eq!(
        related["stored_marker_reported_without_event_for_its_cohort"],
        true
    );
}

#[test]
fn related_events_are_listed_up_to_the_limit_with_their_total() {
    let events: Vec<Value> = (0..LANE_DIAGNOSTICS_LIST_MAX + 5)
        .map(|n| {
            let mut event = creation_event("cohort-1");
            event["id"] = json!(n);
            event
        })
        .collect();

    let related = related_diagnostics(&events, &Ok(marker("cohort-1", true, 100)), &json!({}));

    assert_eq!(related["total"], LANE_DIAGNOSTICS_LIST_MAX + 5);
    assert_eq!(related["shown"], LANE_DIAGNOSTICS_LIST_MAX);
    assert_eq!(related["truncated"], true);
    assert_eq!(
        related["events"].as_array().unwrap().len(),
        LANE_DIAGNOSTICS_LIST_MAX
    );
}

#[test]
fn an_incomplete_evaluation_without_a_terminal_lane_is_unknown_not_lifted() {
    let at_threshold = marker("c", true, 100);
    let none_found_but_partial = json!({
        "evaluated": true,
        "terminal_count": 0,
        "evaluation_complete": false,
    });
    let found_in_the_part = json!({
        "evaluated": true,
        "terminal_count": 1,
        "evaluation_complete": false,
    });
    let none_found_in_all = json!({
        "evaluated": true,
        "terminal_count": 0,
        "evaluation_complete": true,
    });

    assert_eq!(
        suppresses_creation_now(&at_threshold, &none_found_but_partial),
        None
    );
    assert_eq!(
        suppresses_creation_now(&at_threshold, &found_in_the_part),
        Some(true)
    );
    assert_eq!(
        suppresses_creation_now(&at_threshold, &none_found_in_all),
        Some(false)
    );
    assert_eq!(
        suppresses_creation_now(&marker("c", false, 99), &none_found_but_partial),
        Some(false),
        "below the threshold nothing is suppressed whatever was evaluated"
    );
}
