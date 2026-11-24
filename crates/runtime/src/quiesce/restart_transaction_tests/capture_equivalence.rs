//! Exact behaviour of the strict (expected-set) graph capture and its fixture twin.
//!
//! `read_restart_capture_records` reads the stored intents of exactly the runs that were
//! frozen into the restart transaction. These tests pin its result, the rows it must
//! ignore, each way an expected row can fail, and the state the failure leaves behind.
//! Expectations are composed from the inputs (the runs, the recipes, independent SQL
//! reads), not from the output being checked.
use super::*;
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// `session()` of the parent module releases one provider call; this releases one per run.
async fn session_n(
    f: &Fixture,
    recipes: BTreeMap<RunId, RecoveryRecipe>,
) -> crate::RestartCaptureSession {
    f.runtime.begin_planned_restart_quiesce(&recipes).unwrap();
    f.model.release.add_permits(recipes.len());
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    let gate = HostRestartGate::default();
    assert!(gate.transition(1, HostRestartPhase::Sealing));
    let sealed = gate.sealed(&f.runtime, 1, fence).await.unwrap();
    let transaction = create(f);
    sealed
        .begin_capture(&f.runtime, &transaction, recipes)
        .unwrap()
}

fn conn(f: &Fixture) -> Connection {
    Connection::open(&f.config.db_path).unwrap()
}

/// Every column of every intent row, in key order, whatever its stored type.
fn rows(f: &Fixture) -> Vec<Vec<rusqlite::types::Value>> {
    conn(f)
        .prepare("SELECT run_id,incarnation,payload,revision FROM resume_intents ORDER BY run_id")
        .unwrap()
        .query_map([], |r| (0..4).map(|i| r.get(i)).collect())
        .unwrap()
        .map(Result::unwrap)
        .collect()
}

fn revision(f: &Fixture, run: RunId) -> u64 {
    conn(f)
        .query_row(
            "SELECT revision FROM resume_intents WHERE run_id=?1",
            [run.to_string()],
            |r| r.get(0),
        )
        .unwrap()
}

fn edit(conn: &Connection, run: RunId, change: impl FnOnce(&mut Value)) {
    let id = run.to_string();
    let payload: String = conn
        .query_row(
            "SELECT payload FROM resume_intents WHERE run_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let mut value: Value = serde_json::from_str(&payload).unwrap();
    change(&mut value);
    conn.execute(
        "UPDATE resume_intents SET payload=?2 WHERE run_id=?1",
        params![id, value.to_string()],
    )
    .unwrap();
}

/// Rows that belong to no expected run. A strict capture must not even look at them.
fn insert_unrelated_rows(f: &Fixture, template: &ResumeIntent) {
    let conn = conn(f);
    let healthy = |id: RunId, active: bool| {
        let mut v = serde_json::to_value(template).unwrap();
        v["run_id"] = json!(id.to_string());
        v["incarnation"] = json!("unrelated");
        v["active"] = json!(active);
        conn.execute(
            "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'unrelated',?2)",
            params![id.to_string(), v.to_string()],
        )
        .unwrap();
    };
    healthy(f.runtime.reserve_run_id(), true);
    healthy(f.runtime.reserve_run_id(), false);
    // Large, undecodable and relationally broken rows.
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'x',?2)",
        params![
            f.runtime.reserve_run_id().to_string(),
            format!("{{\"filler\":\"{}\"}}", "z".repeat(2 * 1024 * 1024))
        ],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'x','not json')",
        [f.runtime.reserve_run_id().to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,X'0102','{}')",
        [f.runtime.reserve_run_id().to_string()],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(X'07','x','{}')",
        [],
    )
    .unwrap();
}

/// `user_reset_generation` is 1 because a planned-restart quiesce advances each root's
/// reset generation once; the value was characterized on the merge-base.
fn expected_json(f: &Fixture, run: RunId, thread: &str) -> Value {
    let intent = f.intent(run);
    let inputs: Vec<String> = intent
        .initial_input
        .iter()
        .chain(intent.inputs.iter())
        .map(|i| i.id.clone())
        .collect();
    json!({
        "version": 1,
        "run_id": run.to_string(),
        "old_incarnation": intent.incarnation,
        "intent_revision": revision(f, run),
        "parent": null,
        "root": run.to_string(),
        "thread": thread,
        "project": null,
        "role": intent.role,
        "waits": [],
        "deliveries": [],
        "completion": null,
        "auto_wakes": 0,
        "user_reset_generation": 1,
        "input_ids": inputs,
    })
}

/// A record without the two runtime-global counters and the snapshot-derived descriptor,
/// which are asserted separately.
fn projection(record: &RunRecoveryRecord, thread: &str) -> Value {
    let mut v = serde_json::to_value(record).unwrap();
    let object = v.as_object_mut().unwrap();
    object.remove("next_message_id");
    object.remove("completion_generation");
    let recipe = object.get_mut("recipe").unwrap().as_object_mut().unwrap();
    assert_eq!(
        recipe.remove("descriptor").unwrap()["thread_id"],
        json!(thread),
        "the descriptor comes from the stored snapshot"
    );
    assert_eq!(
        Value::Object(std::mem::take(recipe)),
        json!({
            "settings_reference": CORE_RECOVERY_SETTINGS_REFERENCE,
            "policy": serde_json::to_value(ResumePolicy::CoreOwned).unwrap(),
            "goal_id": null,
            "team": null,
            "lane": null,
        })
    );
    v.as_object_mut()
        .unwrap()
        .insert("recipe".into(), json!({}));
    v
}

fn with_empty_recipe(mut v: Value) -> Value {
    v["recipe"] = json!({});
    v
}

#[tokio::test]
async fn strict_capture_returns_exactly_the_expected_runs_and_ignores_every_other_row() {
    let f = Fixture::new(0);
    let a = chat(&f);
    f.model.entered.notified().await;
    let b = f
        .runtime
        .delegate_chat(
            "second-thread",
            Role::Orchestrator,
            "park".into(),
            RunConfig {
                interactive: true,
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.model.entered.notified().await;
    let threads = BTreeMap::from([(a, "thread"), (b, "second-thread")]);
    let mut runs = [a, b];
    runs.sort();
    let mut s = session_n(&f, BTreeMap::from([(a, recipe()), (b, recipe())])).await;
    f.runtime.persist_restart_capture_expected(&mut s).unwrap();
    let wanted: Vec<Value> = runs
        .iter()
        .map(|run| with_empty_recipe(expected_json(&f, *run, threads[run])))
        .collect();
    insert_unrelated_rows(&f, &f.intent(a));
    let before = rows(&f);

    let records = f.runtime.read_restart_capture_records(&s).unwrap();
    assert_eq!(
        records.iter().map(|r| r.run_id).collect::<Vec<_>>(),
        runs,
        "records come back in run id order"
    );
    for ((record, run), wanted) in records.iter().zip(runs).zip(wanted) {
        assert_eq!(projection(record, threads[&run]), wanted);
        assert_eq!(record.next_message_id, records[0].next_message_id);
        assert!(record.completion_generation.is_some());
    }
    assert_eq!(rows(&f), before, "a capture read writes nothing");

    let saved = f.runtime.commit_restart_capture(&mut s).unwrap();
    assert_eq!(saved.phase, RestartPhase::Committed);
    assert_eq!(saved.expected.len(), 2);
    let manifests = db(&f).recovery_manifests().unwrap();
    let mut members: Vec<String> = manifests
        .iter()
        .flat_map(|m| m.record.as_ref().unwrap().members.iter())
        .map(|m| m.run_id.clone())
        .collect();
    members.sort();
    assert_eq!(members, runs.map(|r| r.to_string()));
    f.runtime
        .fence_dispatch_shutdown(|| async { Ok::<_, String>(()) })
        .await
        .unwrap();
}

#[tokio::test]
async fn every_way_an_expected_row_can_fail_invalidates_the_capture_and_changes_nothing_else() {
    type Damage = fn(&Connection, RunId);
    let cases: [(&str, &str, Damage); 8] = [
        (
            "unparsable payload",
            "corrupt expected intent payload",
            |c, run| {
                c.execute(
                    "UPDATE resume_intents SET payload='not json' WHERE run_id=?1",
                    [run.to_string()],
                )
                .unwrap();
            },
        ),
        (
            "identity mismatch",
            "corrupt expected intent payload",
            |c, run| {
                edit(c, run, |v| v["incarnation"] = json!("someone else"));
            },
        ),
        ("relational type", "corrupt expected intent", |c, run| {
            c.execute(
                "UPDATE resume_intents SET incarnation=X'0102' WHERE run_id=?1",
                [run.to_string()],
            )
            .unwrap();
        }),
        ("changed content", "expected intent changed", |c, run| {
            edit(c, run, |v| v["role"] = json!("changed-role"));
        }),
        ("missing row", "expected member missing", |c, run| {
            c.execute(
                "DELETE FROM resume_intents WHERE run_id=?1",
                [run.to_string()],
            )
            .unwrap();
        }),
        ("tombstoned row", "expected member missing", |c, run| {
            c.execute(
                "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,'thread','family')",
                [run.to_string()],
            )
            .unwrap();
        }),
        (
            "unreadable revision",
            "expected member missing",
            |c, run| {
                c.execute(
                    "UPDATE resume_intents SET revision='unreadable' WHERE run_id=?1",
                    [run.to_string()],
                )
                .unwrap();
            },
        ),
        ("scan cannot start", "resume_intents", |c, _| {
            c.execute_batch("DROP TABLE resume_intents").unwrap();
        }),
    ];
    for (name, reason, damage) in cases {
        let f = Fixture::new(0);
        let run = chat(&f);
        f.model.entered.notified().await;
        let mut s = session_n(&f, BTreeMap::from([(run, recipe())])).await;
        f.runtime.persist_restart_capture_expected(&mut s).unwrap();
        insert_unrelated_rows(&f, &f.intent(run));
        let calls = f.model.calls.load(Ordering::SeqCst);
        damage(&conn(&f), run);
        let table_present = name != "scan cannot start";
        let before = table_present.then(|| rows(&f));

        let error = f.runtime.read_restart_capture_records(&s).unwrap_err();
        assert!(error.to_string().contains(reason), "{name}: {error}");
        assert_eq!(tx(&f).phase, RestartPhase::Invalidated, "{name}");
        assert!(!f.runtime.shared.dispatch_tails.capture_valid(), "{name}");
        assert!(
            matches!(f.runtime.quiesce_state(), QuiesceState::Failed { .. }),
            "{name}"
        );
        assert_eq!(f.model.calls.load(Ordering::SeqCst), calls, "{name}");
        assert!(db(&f).recovery_manifests().unwrap().is_empty(), "{name}");
        if let Some(before) = before {
            assert_eq!(rows(&f), before, "{name}: intents are untouched");
        }
    }
}

#[tokio::test]
async fn the_fixture_capture_takes_every_decodable_intent_whose_parents_are_present() {
    let f = Fixture::new(0);
    let a = f.start();
    let b = f.start();
    let recipes = BTreeMap::from([(a, recipe()), (b, recipe())]);
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    let template = f.intent(a);
    let c = conn(&f);
    let extra = |id: RunId, parent: Option<RunId>, active: bool| {
        let mut v = serde_json::to_value(&template).unwrap();
        v["run_id"] = json!(id.to_string());
        v["incarnation"] = json!("extra");
        v["active"] = json!(active);
        v["parent_run_id"] = json!(parent.map(|p| p.to_string()));
        c.execute(
            "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'extra',?2)",
            params![id.to_string(), v.to_string()],
        )
        .unwrap();
    };
    // Inactive rows are still captured; an orphan, a corrupt row and an unreadable
    // identity never are.
    let inactive = f.runtime.reserve_run_id();
    extra(inactive, None, false);
    extra(
        f.runtime.reserve_run_id(),
        Some(f.runtime.reserve_run_id()),
        true,
    );
    c.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'x','not json')",
        [f.runtime.reserve_run_id().to_string()],
    )
    .unwrap();
    c.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(X'07','x','{}')",
        [],
    )
    .unwrap();

    let records = f
        .runtime
        .capture_recovery_records(&f.runtime.fence_dispatch_restart().await.unwrap(), &recipes)
        .unwrap();
    let mut expected = vec![a, b, inactive];
    expected.sort();
    assert_eq!(
        records.iter().map(|r| r.run_id).collect::<Vec<_>>(),
        expected
    );
    for record in &records {
        assert_eq!(record.intent_revision, revision(&f, record.run_id));
        assert_eq!(record.root, record.run_id);
    }
}
