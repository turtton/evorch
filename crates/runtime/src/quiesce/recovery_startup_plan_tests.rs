//! Exact classification of the startup recovery scan (`AgentRuntime::recovery_plan`).
//!
//! The scan decides, for every durable run, whether it is a recovery candidate, a skipped
//! stop, a blocked/corrupt member or nothing at all. These tests fix the **complete,
//! ordered** `RecoveryPlan` for a database that holds every such row kind. The expected
//! value is composed by hand from the inputs (manifest ids returned by the save, run ids
//! reserved by the runtime, the documented `ORDER BY` of the scans). It is never taken
//! from an observed plan.
use super::fixture::Fixture;
use super::*;
use crate::{Role, RunConfig};
use rusqlite::{Connection, params};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn recipe() -> RecoveryRecipe {
    RecoveryRecipe {
        settings_reference: crate::quiesce::CORE_RECOVERY_SETTINGS_REFERENCE.into(),
        policy: ResumePolicy::CoreOwned,
        goal_id: Some("goal".into()),
        team: None,
        lane: None,
        descriptor: None,
    }
}

fn sql(f: &Fixture) -> Connection {
    Connection::open(&f.config.db_path).unwrap()
}

/// Rewrites one stored payload through serde_json. The writer's own trigger still
/// advances the row revision, exactly as a real update would.
fn edit(sql: &Connection, run: RunId, change: impl FnOnce(&mut Value)) {
    let id = run.to_string();
    let payload: String = sql
        .query_row(
            "SELECT payload FROM resume_intents WHERE run_id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    let mut value: Value = serde_json::from_str(&payload).unwrap();
    change(&mut value);
    let changed = sql
        .execute(
            "UPDATE resume_intents SET payload=?2 WHERE run_id=?1",
            params![id, value.to_string()],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

fn stop(sql: &Connection, run: RunId, active: bool, reason: Option<&str>) {
    edit(sql, run, |v| {
        v["active"] = json!(active);
        v["stop_reason"] = reason.map_or(Value::Null, |r| json!(r));
    });
}

fn outcome(manifest: Option<&String>, runs: &[RunId], reason: RecoveryReason) -> RecoveryOutcome {
    RecoveryOutcome {
        manifest_id: manifest.cloned(),
        runs: runs.to_vec(),
        reason,
    }
}

fn authority() -> Vec<RequiredRecoveryAuthority> {
    vec![RequiredRecoveryAuthority::CurrentConfiguration]
}

/// What one durable component is expected to turn into.
enum Expect {
    Component(Vec<RunId>),
    Skipped(RecoveryReason),
    Blocked(RecoveryReason),
}

struct Group {
    name: &'static str,
    runs: Vec<RunId>,
    manifest: String,
    expect: Vec<Expect>,
}

async fn capture(
    f: &Fixture,
    recipes: &BTreeMap<RunId, RecoveryRecipe>,
    after_drain: impl FnOnce(),
) -> Vec<RunRecoveryRecord> {
    let fence = f.runtime.fence_dispatch_restart().await.unwrap();
    f.runtime.drain_dispatch_tails(&fence).await.unwrap();
    after_drain();
    f.runtime
        .capture_recovery_records(&f.runtime.fence_dispatch_restart().await.unwrap(), recipes)
        .unwrap()
}

/// A manifest-less row: what the scan must say about it, by the sort key SQLite uses.
struct Loose {
    sort_key: (bool, Vec<u8>),
    expect: Option<(Vec<RunId>, RecoveryReason)>,
}

impl Loose {
    fn text(id: &str, expect: Option<(Vec<RunId>, RecoveryReason)>) -> Self {
        Self {
            sort_key: (false, id.as_bytes().to_vec()),
            expect,
        }
    }
}

#[tokio::test]
async fn startup_plan_pins_every_row_classification_in_order() {
    let f = Fixture::new(0);
    let healthy = f.start();
    let template = f.intent(healthy);
    let plain = |name: &'static str, f: &Fixture| (name, vec![f.start()]);
    let mut roots: Vec<(&'static str, Vec<RunId>)> = [
        "ok",
        "operator_stop",
        "cancel",
        "cli_disconnect",
        "terminal",
        "terminal_reason_only",
        "revision_conflict",
        "claim",
        "claim_then_stop",
        "bad_json",
        "missing_row",
        "tombstoned_member",
        "identity_mismatch",
        "unknown_version",
        "missing_field",
        "bad_provider_input",
    ]
    .into_iter()
    .map(|name| plain(name, &f))
    .collect();
    let parent = f.runtime.delegate_background(
        Role::Orchestrator,
        "parent".into(),
        RunConfig {
            keep_alive: true,
            ..Default::default()
        },
    );
    let child = f
        .runtime
        .delegate_background_as_child(
            parent,
            Role::Worker,
            "child",
            RunConfig {
                keep_alive: true,
                ..Default::default()
            },
        )
        .unwrap();
    roots.push(("stopped_child", vec![parent, child]));
    let healthy_group = ("healthy_extra", vec![healthy]);
    roots.push(healthy_group);

    // The child stops before the capture and after the dispatch drain, which rewrites
    // intents: its member is recorded as non-resumable.
    let recipes: BTreeMap<RunId, RecoveryRecipe> =
        roots.iter().map(|(_, runs)| (runs[0], recipe())).collect();
    let records = capture(&f, &recipes, || {
        stop(&sql(&f), child, false, Some("operator_stopped"));
    })
    .await;

    let mut groups = Vec::new();
    for (name, runs) in &roots {
        let subset: Vec<_> = records
            .iter()
            .filter(|r| runs.contains(&r.run_id))
            .cloned()
            .collect();
        assert_eq!(subset.len(), runs.len(), "{name}");
        let ids = f.runtime.save_recovery_graph(name, &subset).unwrap();
        assert_eq!(ids.len(), 1, "{name}");
        groups.push(Group {
            name,
            runs: runs.clone(),
            manifest: ids[0].clone(),
            expect: Vec::new(),
        });
    }
    let group =
        |name: &str, groups: &Vec<Group>| groups.iter().position(|g| g.name == name).unwrap();
    let run_of = |name: &str, groups: &Vec<Group>| groups[group(name, groups)].runs[0];

    for name in ["claim", "claim_then_stop"] {
        let g = &groups[group(name, &groups)];
        f._storage
            .handle()
            .claim_recovery_manifest(storage::recovery::RecoveryClaimRequest {
                claim_id: format!("claim-{name}"),
                manifest_id: g.manifest.clone(),
                incarnations: vec![(g.runs[0].to_string(), format!("next-{name}"))],
            })
            .unwrap();
    }

    let conn = sql(&f);
    // Mutations after the manifests are committed.
    stop(
        &conn,
        run_of("operator_stop", &groups),
        true,
        Some("operator_stopped"),
    );
    stop(&conn, run_of("cancel", &groups), false, Some("cancelled"));
    stop(
        &conn,
        run_of("cli_disconnect", &groups),
        false,
        Some("cli_disconnected"),
    );
    stop(&conn, run_of("terminal", &groups), false, None);
    // Active with only a Terminal stop reason is not a stop: it is a changed revision.
    stop(
        &conn,
        run_of("terminal_reason_only", &groups),
        true,
        Some("terminal"),
    );
    edit(&conn, run_of("revision_conflict", &groups), |v| {
        v["resume_requested"] = json!(true);
    });
    // An explicit stop wins over the pending claim and over the revision it advanced.
    stop(
        &conn,
        run_of("claim_then_stop", &groups),
        true,
        Some("operator_stopped"),
    );
    let bad_json = run_of("bad_json", &groups);
    conn.execute(
        "UPDATE resume_intents SET payload='{\"half\"' WHERE run_id=?1",
        [bad_json.to_string()],
    )
    .unwrap();
    let missing = run_of("missing_row", &groups);
    conn.execute(
        "DELETE FROM resume_intents WHERE run_id=?1",
        [missing.to_string()],
    )
    .unwrap();
    let tombstoned = run_of("tombstoned_member", &groups);
    conn.execute(
        "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,'thread','family')",
        [tombstoned.to_string()],
    )
    .unwrap();
    let mismatch = run_of("identity_mismatch", &groups);
    edit(&conn, mismatch, |v| {
        v["incarnation"] = json!("another-incarnation")
    });
    let version = run_of("unknown_version", &groups);
    edit(&conn, version, |v| v["version"] = json!(2));
    let field = run_of("missing_field", &groups);
    edit(&conn, field, |v| {
        v.as_object_mut().unwrap().remove("role");
    });
    let provider = run_of("bad_provider_input", &groups);
    edit(&conn, provider, |v| {
        v["provider_input"] = json!({"profile": 5})
    });

    // Rows no manifest knows. Each is cloned from a real intent so that only the
    // listed property differs from a healthy row.
    let mut loose: Vec<Loose> = Vec::new();
    let insert = |id: RunId, mutate: &dyn Fn(&mut Value)| -> String {
        let mut value = serde_json::to_value(&template).unwrap();
        value["run_id"] = json!(id.to_string());
        let incarnation = format!("incarnation-{id}");
        value["incarnation"] = json!(incarnation);
        mutate(&mut value);
        conn.execute(
            "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,?2,?3)",
            params![id.to_string(), incarnation, value.to_string()],
        )
        .unwrap();
        id.to_string()
    };
    let fresh =
        |expect: Option<RecoveryReason>, mutate: &dyn Fn(&mut Value), loose: &mut Vec<Loose>| {
            let id = f.runtime.reserve_run_id();
            let key = insert(id, mutate);
            loose.push(Loose::text(&key, expect.map(|r| (vec![id], r))));
            id
        };
    fresh(
        Some(RecoveryReason::NoCommittedManifest),
        &|_| {},
        &mut loose,
    );
    fresh(None, &|v| v["active"] = json!(false), &mut loose);
    for reason in [
        "operator_stopped",
        "cancelled",
        "cli_disconnected",
        "terminal",
    ] {
        fresh(
            None,
            &|v| {
                v["active"] = json!(false);
                v["stop_reason"] = json!(reason);
            },
            &mut loose,
        );
    }
    // The scan reports activity, not the stop reason, for a manifest-less row.
    fresh(
        Some(RecoveryReason::NoCommittedManifest),
        &|v| v["stop_reason"] = json!("operator_stopped"),
        &mut loose,
    );
    let gone = f.runtime.reserve_run_id();
    insert(gone, &|_| {});
    conn.execute(
        "INSERT INTO run_tombstones(run_id,thread_id,family_key) VALUES(?1,'thread','family')",
        [gone.to_string()],
    )
    .unwrap();
    for mutate in [
        Box::new(|v: &mut Value| v["incarnation"] = json!("not-the-column"))
            as Box<dyn Fn(&mut Value)>,
        Box::new(|v: &mut Value| v["version"] = json!(9)),
        Box::new(|v: &mut Value| {
            v.as_object_mut().unwrap().remove("role");
        }),
        Box::new(|v: &mut Value| v["provider_input"] = json!({"profile": 5})),
        Box::new(|v: &mut Value| v["inputs"] = json!("not an array")),
    ] {
        fresh(Some(RecoveryReason::CorruptComponent), &*mutate, &mut loose);
    }
    // Invalid JSON text under a readable identity.
    let garbage = f.runtime.reserve_run_id();
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,'x','garbage')",
        [garbage.to_string()],
    )
    .unwrap();
    loose.push(Loose::text(
        &garbage.to_string(),
        Some((vec![garbage], RecoveryReason::CorruptComponent)),
    ));
    // The relational type is wrong, but the identity is readable.
    let typed = f.runtime.reserve_run_id();
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(?1,X'0102','{}')",
        [typed.to_string()],
    )
    .unwrap();
    loose.push(Loose::text(
        &typed.to_string(),
        Some((vec![typed], RecoveryReason::CorruptComponent)),
    ));
    // The identity does not name a run, even though the payload is a healthy intent.
    let mut foreign = serde_json::to_value(&template).unwrap();
    foreign["incarnation"] = json!("foreign");
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES('not-a-run-id','foreign',?1)",
        [foreign.to_string()],
    )
    .unwrap();
    loose.push(Loose::text(
        "not-a-run-id",
        Some((vec![], RecoveryReason::CorruptComponent)),
    ));
    // Not even the identity can be read: BLOB sorts after every TEXT key.
    conn.execute(
        "INSERT INTO resume_intents(run_id,incarnation,payload) VALUES(X'05','x','{}')",
        [],
    )
    .unwrap();
    loose.push(Loose {
        sort_key: (true, vec![5]),
        expect: Some((vec![], RecoveryReason::CorruptComponent)),
    });

    // The corrupt-member groups are additionally reported by the manifest-less pass,
    // because that pass looks at every stored row.
    for name in [
        "bad_json",
        "identity_mismatch",
        "unknown_version",
        "missing_field",
        "bad_provider_input",
    ] {
        let run = run_of(name, &groups);
        loose.push(Loose::text(
            &run.to_string(),
            Some((vec![run], RecoveryReason::CorruptComponent)),
        ));
    }

    let expectations: [(&str, Expect); 18] = [
        ("healthy_extra", Expect::Component(vec![healthy])),
        ("ok", Expect::Component(vec![run_of("ok", &groups)])),
        (
            "operator_stop",
            Expect::Skipped(RecoveryReason::OperatorStopped),
        ),
        ("cancel", Expect::Skipped(RecoveryReason::Cancelled)),
        (
            "cli_disconnect",
            Expect::Skipped(RecoveryReason::CliDisconnected),
        ),
        ("terminal", Expect::Skipped(RecoveryReason::Terminal)),
        (
            "terminal_reason_only",
            Expect::Skipped(RecoveryReason::IntentRevisionConflict),
        ),
        (
            "revision_conflict",
            Expect::Skipped(RecoveryReason::IntentRevisionConflict),
        ),
        ("claim", Expect::Blocked(RecoveryReason::PendingClaim)),
        (
            "claim_then_stop",
            Expect::Skipped(RecoveryReason::OperatorStopped),
        ),
        (
            "bad_json",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "missing_row",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "tombstoned_member",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "identity_mismatch",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "unknown_version",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "missing_field",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        (
            "bad_provider_input",
            Expect::Blocked(RecoveryReason::CorruptComponent),
        ),
        ("stopped_child", Expect::Component(vec![parent])),
    ];
    for (name, expect) in expectations {
        let index = group(name, &groups);
        groups[index].expect.push(expect);
    }
    // The stopped child is a skipped endpoint next to the resumed parent.
    let pair = group("stopped_child", &groups);
    groups[pair]
        .expect
        .push(Expect::Skipped(RecoveryReason::OperatorStopped));

    // Compose the expected plan: manifests by manifest id, then manifest-less rows by key.
    let mut expected = RecoveryPlan::default();
    let mut ordered: Vec<&Group> = groups.iter().collect();
    ordered.sort_by(|a, b| a.manifest.cmp(&b.manifest));
    for g in ordered {
        for e in &g.expect {
            match e {
                Expect::Component(runs) => expected.components.push(RecoveryComponent {
                    manifest_id: g.manifest.clone(),
                    runs: runs.clone(),
                    required_authority: authority(),
                }),
                Expect::Skipped(reason) => {
                    // The stopped child is reported alone; every other skip names the
                    // whole component (a single run).
                    let runs: Vec<RunId> = if g.name == "stopped_child" {
                        vec![child]
                    } else {
                        g.runs.clone()
                    };
                    expected
                        .report
                        .skipped
                        .push(outcome(Some(&g.manifest), &runs, *reason));
                }
                Expect::Blocked(reason) => {
                    expected
                        .report
                        .blocked
                        .push(outcome(Some(&g.manifest), &g.runs, *reason))
                }
            }
        }
    }
    loose.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));
    for l in loose {
        let Some((runs, reason)) = l.expect else {
            continue;
        };
        let target = if reason == RecoveryReason::NoCommittedManifest {
            &mut expected.report.skipped
        } else {
            &mut expected.report.blocked
        };
        target.push(outcome(None, &runs, reason));
    }

    let plan = f.runtime.recovery_plan().unwrap();
    assert_eq!(plan, expected);
    assert!(plan.report.recovered.is_empty());
    // The scan never mutates what it reads.
    let again = f.runtime.recovery_plan().unwrap();
    assert_eq!(again, expected);
}
