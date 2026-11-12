//! `lane_diagnostics` of `inspect run`, read from a ledger the production writer made.
//! The read side opens the database read-only and must leave every row as it was.
use super::*;
use serde_json::{Value, json};
use storage::lane_instructions::{
    LaneInstructionAction, LaneInstructionActor, LaneInstructionRequest,
};

fn lead_run(f: &Fixture) {
    f.h().upsert_run_context(&snapshot().record).unwrap();
}
fn lane_run(f: &Fixture, run: &str) {
    let mut record = snapshot().record;
    record.run_id = run.into();
    record.name = "lane".into();
    record.config_json = r#"{"thread_id":"thread-1"}"#.into();
    f.h().upsert_run_context(&record).unwrap();
}
fn section(f: &Fixture, run: &str) -> Value {
    Database::open_read_only(&f.config.db_path)
        .unwrap()
        .inspect_lane_diagnostics(run, &[])
        .unwrap()
}
fn first_lane(section: &Value) -> &Value {
    &section["lanes"]["items"][0]
}
fn kinds(lane: &Value) -> Vec<String> {
    lane["attention"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["kind"]["kind"].as_str().unwrap().to_owned())
        .collect()
}
fn row_counts(f: &Fixture) -> Vec<u64> {
    [
        "lanes",
        "lane_reports",
        "lane_notifications",
        "lane_stall_episodes",
        "lane_recovery_episodes",
        "lane_delivery_destinations",
        "lane_closes",
        "lane_instructions",
    ]
    .iter()
    .map(|table| f.count(table))
    .collect()
}
fn guard_json(f: &Fixture) -> String {
    f.sql()
        .query_row("SELECT wake_guard_json FROM lanes", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn a_stalled_lane_shows_its_episode_nudge_state_and_the_actions_open_to_the_lead() {
    let f = Fixture::new();
    let owner = started(&f);
    lead_run(&f);
    let mut stall = observation(&f, &owner);
    stall.classification = LaneStallClassification::NoReport;
    let episode = f.h().observe_lane_stall(stall.clone()).unwrap();
    let nudge = f
        .h()
        .request_lane_nudge(stall, episode.episode_id.clone(), episode.revision)
        .unwrap();
    let before = (row_counts(&f), guard_json(&f));

    let section = section(&f, "lead-root");

    assert_eq!((row_counts(&f), guard_json(&f)), before, "inspect wrote");
    assert_eq!(section["scope"]["managed_lanes_attributed"], true);
    let lane = first_lane(&section);
    assert_eq!(lane["lane_id"], "lane-1");
    assert_eq!(
        lane["relations"],
        json!(["managed_by_this_run", "opened_by_this_run"])
    );
    assert_eq!(lane["stall"]["total"], 1);
    let shown = &lane["stall"]["episodes"][0];
    assert_eq!(shown["episode_id"], episode.episode_id);
    assert_eq!(shown["classification"], "no_report");
    assert_eq!(shown["nudge_state"], "pending");
    assert_eq!(shown["notification_id"], json!(nudge.notification_id));
    assert_eq!(kinds(lane), ["stall"]);
    assert_eq!(
        lane["attention"]["items"][0]["kind"]["episode_id"],
        shown["episode_id"]
    );
}

#[test]
fn ready_close_and_instructions_are_shown_as_sequences_and_revisions_without_text() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let ready_only = section(&f, "lead-root");
    let lane = first_lane(&ready_only);
    assert_eq!(lane["ready"]["ready_seq"], 1);
    assert_eq!(lane["ready"]["report_brief_revision"], 1);
    assert_eq!(kinds(lane), ["ready_unclosed"]);
    assert_eq!(lane["close"], Value::Null);
    assert_eq!(lane["instructions"]["latest"], Value::Null);

    f.h()
        .apply_lane_instruction(
            LaneInstructionRequest {
                operation_id: "supplement-1".into(),
                lane_id: "lane-1".into(),
                expected_management_revision: 1,
                expected_brief_revision: 1,
                action: LaneInstructionAction::Supplement {
                    message: "private supplement text".into(),
                },
            },
            LaneInstructionActor {
                thread_id: "lead".into(),
                root_run_id: "lead-root".into(),
            },
            4,
        )
        .unwrap();
    f.h().close_lane(close_now(&f)).unwrap();

    let closed = section(&f, "lead-root");
    let lane = first_lane(&closed);
    assert_eq!(lane["instructions"]["latest"]["instruction_seq"], 1);
    assert_eq!(
        lane["instructions"]["latest"]["accepted_management_revision"],
        1
    );
    assert_eq!(lane["instructions"]["latest"]["brief_revision_after"], 1);
    assert_eq!(
        lane["close"],
        json!({"ready_seq": 1, "closed_revision": lane["request_revision"], "management_revision": 1, "brief_revision": 1})
    );
    assert!(kinds(lane).is_empty(), "a closed lane needs nothing more");
    let text = closed.to_string();
    for private in [
        "private supplement text",
        "accepted on my own judgement",
        "implement",
        "result",
    ] {
        assert!(!text.contains(private), "{private} leaked: {text}");
    }
}

#[test]
fn a_suppressed_wake_guard_and_restore_episode_are_shown_with_counts_and_actions() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Attached);
    f.create(1, 0, LaneDeliveryBinding::Bound);
    f.create(1, 1, LaneDeliveryBinding::Bound);
    lead_thread(&f);
    lead_run(&f);
    let mut episode = None;
    for at in [10, 20, 30] {
        let c = claim(&f.h(), at);
        let attempt = f.h().prepare_lane_recovery(c.clone(), at).unwrap();
        episode = Some(
            f.h()
                .record_lane_recovery_failure(c.clone(), attempt, at + 1)
                .unwrap()
                .episode,
        );
        f.h()
            .nack_lane_delivery(c, LaneDeliveryNack::RestoreFailed, at + 2)
            .unwrap();
    }
    let episode = episode.unwrap();
    assert_eq!(episode.state, LaneRecoveryState::Suppressed);
    // A suppressed wake guard holds back further claims, so it is reached last.
    let mut guard = LaneWakeGuard::default();
    for n in 0..LANE_NO_PROGRESS_LIMIT {
        guard = f
            .h()
            .update_lane_wake_guard(
                "lane-1".into(),
                guard.revision,
                LaneGuardSignal::WakeBatch {
                    batch_id: format!("batch-{n}"),
                },
            )
            .unwrap();
    }
    assert!(guard.suppressed);

    let section = section(&f, "lead-root");

    let lane = first_lane(&section);
    assert_eq!(lane["wake_guard"]["suppressed"], true);
    assert_eq!(
        lane["wake_guard"]["no_progress_batches"],
        LANE_NO_PROGRESS_LIMIT
    );
    let mut attention = kinds(lane);
    attention.sort();
    assert_eq!(
        attention,
        [
            "question_pending",
            "question_pending",
            "restore_failed",
            "wake_suppressed"
        ]
    );
    let restore = lane["attention"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["kind"]["kind"] == "restore_failed")
        .unwrap();
    assert_eq!(restore["kind"]["suppressed"], true);
    assert_eq!(restore["kind"]["attempts"], 3);
    assert_eq!(restore["available_actions"], json!(["inspect", "resume"]));
    let destination = &section["delivery"]["destinations"][0];
    assert_eq!(destination["thread_id"], "lead");
    assert_eq!(
        destination["recovery"]["active_episode_id"],
        episode.episode_id
    );
    let shown = &destination["recovery"]["episodes"][0];
    assert_eq!(shown["state"], "suppressed");
    assert_eq!(shown["attempts"], 3);
    assert_eq!(shown["saved_root_run_is_this_run"], true);
    assert_eq!(
        destination["pending_notifications"][0]["count"].as_u64(),
        Some(2),
        "two question notices wait for the lead"
    );
}

#[test]
fn a_past_run_is_not_given_the_threads_current_management() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);
    f.h()
        .upsert_threads_host(&[storage::threads::ThreadUpsert {
            thread_id: "lead".into(),
            root_run_id: Some("lead-root-2".into()),
            chat_role: Some("Orchestrator".into()),
            ..Default::default()
        }])
        .unwrap();

    let section = section(&f, "lead-root");

    assert_eq!(section["scope"]["run_is_current_root_of_thread"], false);
    assert_eq!(section["scope"]["thread_root_run_id"], "lead-root-2");
    assert_eq!(section["scope"]["managed_lanes_attributed"], false);
    let lane = first_lane(&section);
    assert_eq!(lane["relations"], json!(["opened_by_this_run"]));
    assert_eq!(section["delivery"]["destinations"], json!([]));
}

#[test]
fn a_run_inside_a_lane_thread_sees_its_lane_and_the_lead_delivery_state() {
    let f = Fixture::new();
    started(&f);
    lane_run(&f, "run-1");
    lane_run(&f, "run-0");

    let current = section(&f, "run-1");
    let past = section(&f, "run-0");

    assert_eq!(current["scope"]["thread_id"], "thread-1");
    let lane = first_lane(&current);
    assert_eq!(lane["lane_id"], "lane-1");
    assert_eq!(lane["relations"], json!(["lane_root_run"]));
    let destinations: Vec<_> = current["delivery"]["destinations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|destination| destination["thread_id"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(destinations, ["lead", "thread-1"]);
    assert_eq!(
        current["delivery"]["destinations"][1]["owner_is_this_run"],
        true
    );
    assert_eq!(first_lane(&past)["relations"], json!(["lane_thread_run"]));
    assert_eq!(
        past["delivery"]["destinations"]
            .as_array()
            .unwrap()
            .iter()
            .map(|destination| destination["thread_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["lead"],
        "a past run of the lane is not shown the lane thread's own destination"
    );
}

#[test]
fn a_deleted_run_and_an_unrelated_run_show_nothing_of_the_ledger() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);

    let unrelated = section(&f, "somebody-else");
    assert_eq!(unrelated["lanes"]["total"], 0);
    assert_eq!(unrelated["lanes"]["items"], json!([]));

    f.sql()
        .execute(
            "INSERT INTO run_tombstones(run_id, thread_id, family_key) VALUES ('ghost-run','ghost-thread','family')",
            [],
        )
        .unwrap();
    assert_eq!(section(&f, "ghost-run"), Value::Null);
}

#[test]
fn the_current_state_of_a_stored_uncollected_cohort_follows_the_ledger() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);
    let ids = vec!["lane-1".to_owned(), "gone".to_owned()];
    let read = |f: &Fixture| {
        Database::open_read_only(&f.config.db_path)
            .unwrap()
            .inspect_lane_diagnostics("lead-root", &ids)
            .unwrap()["uncollected_current"]
            .clone()
    };

    let working = read(&f);
    assert_eq!(working["evaluated"], true);
    assert_eq!(working["managed_count"], 1);
    assert_eq!(working["terminal_count"], 0);
    assert_eq!(
        working["lanes"][1],
        json!({"lane_id": "gone", "exists": false})
    );

    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();
    let ready = read(&f);
    assert_eq!(ready["terminal_count"], 1);
    assert_eq!(ready["lanes"][0]["terminal_for_current_management"], true);
    assert_eq!(ready["lanes"][0]["latest_report_status"], "ready");
}

#[test]
fn everything_read_in_one_snapshot_comes_from_one_commit() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);
    let db = Database::open_read_only(&f.config.db_path).unwrap();

    let (before, during) = db
        .read_snapshot(|db| {
            let before = db.inspect_lane_diagnostics("lead-root", &[])?;
            // A writer on its own connection commits while the snapshot is open.
            f.h()
                .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
                .unwrap();
            let during = db.inspect_lane_diagnostics("lead-root", &[])?;
            Ok((before, during))
        })
        .unwrap();

    assert_eq!(first_lane(&before)["latest_report"], Value::Null);
    assert_eq!(
        before, during,
        "the commit made inside the snapshot leaked in"
    );
    let after = db.inspect_lane_diagnostics("lead-root", &[]).unwrap();
    assert_eq!(first_lane(&after)["latest_report"]["status"], "ready");
}

#[test]
fn attention_is_cut_at_the_list_limit_and_counts_every_kind_in_its_total() {
    let f = Fixture::new();
    started(&f);
    lead_run(&f);
    for question in 0..25 {
        f.create(1, question, LaneDeliveryBinding::Bound);
    }
    f.h()
        .append_lane_report("lane-1", 1, report(LaneStatus::Ready), 3)
        .unwrap();

    let section = section(&f, "lead-root");

    let lane = first_lane(&section);
    let attention = &lane["attention"];
    assert_eq!(attention["eligible"], true);
    assert_eq!(
        attention["total"], 26,
        "25 questions and the unclosed Ready"
    );
    assert_eq!(
        attention["shown"],
        storage::inspect::LANE_DIAGNOSTICS_LIST_MAX
    );
    assert_eq!(attention["truncated"], true);
    assert!(
        kinds(lane).contains(&"ready_unclosed".to_owned()),
        "the newest item is among those shown"
    );
}

#[test]
fn a_detached_lane_has_no_attention_or_close_to_read() {
    let f = Fixture::new();
    f.lane(1, LanePolicy::Detached);
    lane_run(&f, "run-1");

    let section = section(&f, "run-1");

    let lane = first_lane(&section);
    assert_eq!(lane["attention"]["eligible"], false);
    assert_eq!(lane["attention"]["items"], json!([]));
    assert_eq!(lane["close"], Value::Null);
}
