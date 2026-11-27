//! `lane_send` to several lanes: every lane is accepted or refused on its own, an
//! identical call settles only what is left, and invalid input stores nothing.
use super::*;

fn target(lane_id: &str, management: u64, brief: u64) -> Value {
    json!({
        "lane_id": lane_id,
        "expected_management_revision": management,
        "expected_brief_revision": brief,
    })
}

fn send(operation_id: &str, message: &str, targets: Vec<Value>) -> Value {
    json!({"operation_id": operation_id, "message": message, "targets": targets})
}

fn revisions(fx: &Fixture, lane_id: &str) -> (u64, u64) {
    let lane = fx.store.handle().lane(lane_id).unwrap().unwrap();
    (lane.management_revision, lane.brief_revision)
}

fn stored(fx: &Fixture, lane_id: &str) -> usize {
    fx.store
        .handle()
        .lane_instructions(lane_id, 0, 10)
        .unwrap()
        .len()
}

fn is_error(fx: &Fixture, call_id: &str) -> bool {
    fx.model.errors.lock().unwrap().contains(call_id)
}

async fn three_lanes(fx: &mut Fixture) -> [String; 3] {
    let mut lanes = Vec::new();
    for n in 0..3 {
        lanes.push(lane_id(&fx.open(&format!("lane-{n}")).await));
    }
    lanes.try_into().unwrap()
}

#[tokio::test]
async fn one_refused_lane_does_not_stop_the_others_and_an_identical_retry_settles_only_it() {
    let mut fx = Fixture::new(None).await;
    let [a, b, c] = three_lanes(&mut fx).await;
    let ((ma, ba), (mb, bb), (mc, bc)) =
        (revisions(&fx, &a), revisions(&fx, &b), revisions(&fx, &c));
    // The middle lane is expected one management revision ahead of where it is.
    let call_args = || {
        send(
            "supplement-1",
            "check the boundary cases",
            vec![
                target(&a, ma, ba),
                target(&b, mb + 1, bb),
                target(&c, mc, bc),
            ],
        )
    };
    let first = fx.one("first", "lane_send", call_args()).await;
    assert!(!is_error(&fx, "first"), "a partial success is not an error");
    let results = first["results"].as_array().unwrap();
    assert_eq!(first["operation_id"], "supplement-1");
    assert_eq!(
        results
            .iter()
            .map(|r| r["lane_id"].clone())
            .collect::<Vec<_>>(),
        [json!(a), json!(b), json!(c)],
        "results follow the order of targets"
    );
    assert_eq!(results[0]["status"], "accepted", "{first}");
    assert_eq!(results[1]["status"], "error", "{first}");
    assert_eq!(results[1]["error"]["code"], "stale_management_revision");
    assert_eq!(results[2]["status"], "accepted", "{first}");
    assert_eq!(
        results[0]["operation"]["operation_id"],
        format!("supplement-1:{a}")
    );
    assert_eq!(results[0]["operation"]["replayed"], false);
    assert!(first["note"].as_str().unwrap().contains("not read"));
    assert!(
        first.get("message").is_none(),
        "the text is not echoed back"
    );
    assert_eq!(
        (stored(&fx, &a), stored(&fx, &b), stored(&fx, &c)),
        (1, 0, 1)
    );

    // What made the middle lane fail goes away without touching the arguments.
    let policy = json!({"operation_id":"policy-1","lane_id":b,"expected_management_revision":mb,"policy":"summary"});
    let changed = fx.one("policy", "lane_set_policy", policy).await;
    assert!(changed.get("code").is_none(), "{changed}");
    assert_eq!(revisions(&fx, &b).0, mb + 1);

    let retry = fx.one("retry", "lane_send", call_args()).await;
    assert!(!is_error(&fx, "retry"));
    let again = retry["results"].as_array().unwrap();
    for (n, original) in [(0, &results[0]), (2, &results[2])] {
        assert_eq!(again[n]["status"], "accepted", "{retry}");
        assert_eq!(again[n]["operation"]["replayed"], true);
        for field in [
            "instruction_seq",
            "notification_id",
            "accepted_management_revision",
        ] {
            assert_eq!(again[n]["operation"][field], original["operation"][field]);
        }
    }
    assert_eq!(again[1]["status"], "accepted", "{retry}");
    assert_eq!(again[1]["operation"]["replayed"], false);
    assert_eq!(again[1]["operation"]["instruction_seq"], 1);
    assert_eq!(
        (stored(&fx, &a), stored(&fx, &b), stored(&fx, &c)),
        (1, 1, 1)
    );
    for lane in [&a, &b, &c] {
        assert_eq!(
            revisions(&fx, lane).1,
            1,
            "a supplement never revises the brief"
        );
    }
    fx.shutdown().await;
}

#[tokio::test]
async fn a_call_that_stores_nothing_is_an_error_in_the_same_result_shape() {
    let mut fx = Fixture::new(None).await;
    let [a, ..] = three_lanes(&mut fx).await;
    let (m, b) = revisions(&fx, &a);
    let refused = send("supplement-2", "text", vec![target(&a, m + 1, b)]);
    let body = fx.one("refused", "lane_send", refused).await;
    assert!(is_error(&fx, "refused"));
    assert_eq!(body["results"][0]["status"], "error", "{body}");
    assert_eq!(
        body["results"][0]["error"]["code"],
        "stale_management_revision"
    );
    assert_eq!(body["operation_id"], "supplement-2");
    assert_eq!(stored(&fx, &a), 0);
    fx.shutdown().await;
}

#[tokio::test]
async fn invalid_input_is_refused_whole_before_any_lane_is_touched() {
    let mut fx = Fixture::new(None).await;
    let [a, b, _] = three_lanes(&mut fx).await;
    let (ma, ba) = revisions(&fx, &a);
    let (mb, bb) = revisions(&fx, &b);
    let long_operation = "o".repeat(250);
    let cases = [
        (
            "duplicate",
            "dup-op",
            vec![
                target(&a, ma, ba),
                target(&b, mb, bb),
                target(&a, ma + 5, ba),
            ],
            "duplicate_lane_id",
        ),
        (
            "too-long",
            long_operation.as_str(),
            vec![target(&a, ma, ba), target(&b, mb, bb)],
            "derived_operation_id_too_long",
        ),
    ];
    for (id, operation_id, targets, code) in cases {
        let args = send(operation_id, "text", targets);
        let refused = fx.one(id, "lane_send", args).await;
        assert!(is_error(&fx, id), "{id}");
        assert_eq!(refused["code"], code, "{id}: {refused}");
    }
    assert_eq!((stored(&fx, &a), stored(&fx, &b)), (0, 0));
    fx.shutdown().await;
}

#[tokio::test]
async fn every_lane_stores_under_the_same_derived_id_whatever_the_count_or_order() {
    let mut fx = Fixture::new(None).await;
    let [a, b, _] = three_lanes(&mut fx).await;
    let (ma, ba) = revisions(&fx, &a);
    let (mb, bb) = revisions(&fx, &b);
    let alone = send("same-id", "one text", vec![target(&a, ma, ba)]);
    let alone = fx.one("alone", "lane_send", alone).await;
    assert_eq!(
        alone["results"][0]["operation"]["operation_id"],
        format!("same-id:{a}"),
        "a single target is derived too"
    );
    let seq = alone["results"][0]["operation"]["instruction_seq"].clone();
    let both = send(
        "same-id",
        "one text",
        vec![target(&b, mb, bb), target(&a, ma, ba)],
    );
    let both = fx.one("both", "lane_send", both).await;
    assert_eq!(both["results"][0]["lane_id"], json!(b));
    assert_eq!(both["results"][0]["operation"]["replayed"], false);
    assert_eq!(both["results"][1]["operation"]["replayed"], true);
    assert_eq!(both["results"][1]["operation"]["instruction_seq"], seq);
    assert_eq!((stored(&fx, &a), stored(&fx, &b)), (1, 1));
    fx.shutdown().await;
}

#[tokio::test]
async fn a_changed_request_for_an_accepted_lane_conflicts_and_a_refused_lane_can_be_corrected() {
    let mut fx = Fixture::new(None).await;
    let [a, b, _] = three_lanes(&mut fx).await;
    let (ma, ba) = revisions(&fx, &a);
    let (mb, bb) = revisions(&fx, &b);
    let first = send(
        "conflict-op",
        "original text",
        vec![target(&a, ma, ba), target(&b, mb + 3, bb)],
    );
    let first = fx.one("first", "lane_send", first).await;
    assert_eq!(first["results"][0]["status"], "accepted", "{first}");
    assert_eq!(first["results"][1]["status"], "error", "{first}");

    // Same ID, other text: the accepted lane refuses it; the lane that stored nothing
    // takes it with its revisions corrected.
    let changed = send(
        "conflict-op",
        "other text",
        vec![target(&a, ma, ba), target(&b, mb, bb)],
    );
    let changed = fx.one("changed", "lane_send", changed).await;
    assert_eq!(changed["results"][0]["status"], "error", "{changed}");
    assert_eq!(
        changed["results"][0]["error"]["code"],
        "operation_id_conflict"
    );
    assert_eq!(changed["results"][1]["status"], "accepted", "{changed}");
    assert_eq!((stored(&fx, &a), stored(&fx, &b)), (1, 1));
    fx.shutdown().await;
}

#[tokio::test]
async fn a_lead_that_lost_a_lane_cannot_replay_its_earlier_acceptance() {
    let mut fx = Fixture::new(None).await;
    let [a, b, _] = three_lanes(&mut fx).await;
    let (ma, ba) = revisions(&fx, &a);
    let (mb, bb) = revisions(&fx, &b);
    let call_args = || {
        send(
            "lost-op",
            "text",
            vec![target(&a, ma, ba), target(&b, mb, bb)],
        )
    };
    let first = fx.one("first", "lane_send", call_args()).await;
    assert_eq!(first["results"][0]["status"], "accepted", "{first}");
    let detach = json!({"operation_id":"detach-a","lane_id":a,"expected_management_revision":ma});
    fx.one("detach", "lane_detach", detach).await;
    let retry = fx.one("retry", "lane_send", call_args()).await;
    assert_eq!(retry["results"][0]["status"], "error", "{retry}");
    assert_eq!(retry["results"][0]["error"]["code"], "not_current_lead");
    assert_eq!(
        retry["results"][1]["status"], "accepted",
        "the other lane still replays"
    );
    assert_eq!(retry["results"][1]["operation"]["replayed"], true);
    assert_eq!((stored(&fx, &a), stored(&fx, &b)), (1, 1));
    fx.shutdown().await;
}
