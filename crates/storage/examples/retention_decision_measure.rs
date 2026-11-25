//! Measurement of the plan-C archive decision SQL on a synthetic database (C-34).
//! Not a CI test: results are recorded in the PR, never asserted on time.
//!
//! Declared before the first run (nothing below was tuned to the numbers):
//! * budgets: R = 1,024 family threads+runs, P = 16 tail rows per run, Q = 512 tail rows per
//!   decision, K = 128 goal decodes per decision, 2,097,152 decoded bytes per decision.
//! * decision SQL set on the largest (624-run) family, 30 fresh connections per scenario:
//!   p95 <= 20 ms and max <= 50 ms (the whole tick keeps the host-mutex limits p95 <= 50 ms,
//!   max <= 100 ms; the SQL is only one part of it).
//! * every `events` statement plans as `SEARCH ... USING INDEX <pinned>`: no `SCAN events`,
//!   no temp b-tree, FULLSCAN_STEP = 0 on events statements.
//! * building the goal partial index on 1M+ events: <= 5 s.
//! * append of a goal or non-goal event with the new indexes: <= +0.05 ms mean per insert.
//!
//! `RDM_TRIALS=30 cargo run --release --offline -p storage --example
//! retention_decision_measure -- <work-dir>` (work dir must not hold data of value).

use event_bus::{
    AgentRunPhase, EventKind, LifecycleEvent, OrchestratorEvent, ThreadGoalPhase,
    ThreadGoalSnapshot, ThreadGoalUsage,
};
use rusqlite::{Connection, OpenFlags, StatementStatus, params};
use std::error::Error;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};
use storage::thread_retention::decision::sql;
use storage::{Database, StorageConfig};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

const DAY_NS: i64 = 86_400 * 1_000_000_000;
const THREADS: usize = 2_000;
const BIG_THREADS: usize = 40;
const BIG_RUNS: usize = 624;
const GOAL_EVENTS: i64 = 130_000;
const OTHER_EVENTS: i64 = 900_000;
const P: usize = 16;
const Q: usize = 512;
const K: usize = 128;
const BYTES: usize = 2_097_152;

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn now_ns() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    )
    .unwrap()
}

fn pct(values: &mut [f64], fraction: f64) -> f64 {
    values.sort_by(|a, b| a.partial_cmp(b).unwrap());
    values[((values.len() as f64 - 1.0) * fraction) as usize]
}

fn goal(id: &str, thread: &str, root: &str, phase: ThreadGoalPhase) -> EventKind {
    OrchestratorEvent::ThreadGoalUpdated {
        snapshot: ThreadGoalSnapshot {
            goal_id: id.into(),
            thread_id: thread.into(),
            root_run_id: root.into(),
            related_root_run_ids: vec![],
            objective: "o".repeat(500),
            criteria: vec!["c".repeat(120), "d".repeat(120)],
            checks: vec![],
            phase,
            review_enabled: false,
            checks_paused: false,
            work_stopped: false,
            epoch: 0,
            review_round: 0,
            findings: vec![],
            reason: None,
            usage: ThreadGoalUsage::default(),
            max_review_rounds: 1,
            max_tokens: None,
            original_request: "r".repeat(200),
        },
    }
    .into()
}

fn state_change(run: &str, from: AgentRunPhase, to: AgentRunPhase) -> EventKind {
    LifecycleEvent::AgentRunStateChanged {
        run_id: run.into(),
        from,
        to,
        reason: None,
        waiting: None,
    }
    .into()
}

/// (category, subkind, per mille, payload padding)
const MIX: [(&str, &str, u32, usize); 11] = [
    ("Message", "MessagePart", 120, 180),
    ("Message", "MessageCompleted", 40, 400),
    ("Tool", "ToolStarted", 150, 250),
    ("Tool", "ToolCompleted", 150, 450),
    ("Message", "ReasoningDelta", 150, 200),
    ("Provider", "RequestStarted", 100, 300),
    ("Provider", "RequestCompleted", 100, 300),
    ("Lifecycle", "TurnCompleted", 20, 120),
    ("Lifecycle", "AgentRunStateChanged", 30, 0),
    ("AgentMessage", "Delivered", 10, 300),
    ("Message", "MessageDelta", 130, 150),
];

struct Fixture {
    big_threads: Vec<String>,
    big_runs: Vec<String>,
    last_ids: Vec<(String, i64)>,
}

fn insert_event(
    stmt: &mut rusqlite::Statement<'_>,
    at: i64,
    cat: &str,
    sub: &str,
    run: &str,
    payload: &str,
) -> Result<()> {
    stmt.execute(params![at, cat, payload, sub, run])?;
    Ok(())
}

fn build(config: &StorageConfig) -> Result<Fixture> {
    drop(Database::open(config)?);
    let conn = Connection::open(&config.db_path)?;
    conn.execute(
        "INSERT INTO sessions (id, status, created_at_ns, updated_at_ns) VALUES ('s','running',0,0)",
        [],
    )?;
    let now = now_ns();
    let mut rng = Rng(7);
    // Runs and threads: a 40-thread, 624-run family plus 1,960 ordinary threads.
    let mut runs: Vec<(String, String, String)> = vec![];
    let mut next = 0usize;
    for t in 0..THREADS {
        let count = if t < BIG_THREADS {
            BIG_RUNS / BIG_THREADS + usize::from(t < BIG_RUNS % BIG_THREADS)
        } else {
            1 + rng.below(3)
        };
        let root = format!("run-{next}");
        for _ in 0..count {
            runs.push((format!("run-{next}"), format!("thread-{t}"), root.clone()));
            next += 1;
        }
    }
    let tx = conn.unchecked_transaction()?;
    {
        let mut tr = tx.prepare(
            "INSERT INTO thread_runs(run_id,thread_id,root_run_id,depth) VALUES(?1,?2,?3,0)",
        )?;
        let mut rc = tx.prepare(
            "INSERT INTO run_contexts(run_id,role,name,parent_run_id,config_json,checkpoints_json,terminal_phase,restorable,updated_at_ns) \
             VALUES(?1,'worker','w',NULL,'{}','[]','done',1,?2)",
        )?;
        for (run, thread, root) in &runs {
            tr.execute(params![run, thread, root])?;
            rc.execute(params![run, now - 60 * DAY_NS])?;
        }
    }
    // Event-less conversation runs (legacy fallback candidates) in the big family.
    let eventless: Vec<String> = (0..30).map(|i| format!("run-{}", i * 20 + 7)).collect();
    // Goal-bearing roots: every thread's root run of the first 1,000 threads.
    let goal_roots: Vec<(String, String)> = {
        let mut seen = std::collections::BTreeMap::new();
        for (_, thread, root) in &runs {
            seen.entry(root.clone()).or_insert(thread.clone());
        }
        seen.into_iter().take(1_000).collect()
    };
    let span = 90 * DAY_NS;
    let total = OTHER_EVENTS + GOAL_EVENTS;
    let mut ends = std::collections::HashMap::new();
    let mut starts = std::collections::HashMap::new();
    for (run, _, _) in &runs {
        starts.insert(run.clone(), rng.below(80) as i64);
        ends.insert(run.clone(), 0i64);
    }
    let mut last_event: std::collections::HashMap<String, i64> = Default::default();
    {
        let mut insert = tx.prepare(
            "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) \
             VALUES('s',1,1,?1,?2,?3,?4,?5)",
        )?;
        let mut i: i64 = 0;
        let mut goal_n: i64 = 0;
        while i < total {
            let at = now - 120 * DAY_NS + i * span / total;
            let day = (at - (now - 120 * DAY_NS)) / DAY_NS;
            let is_goal = goal_n * total < GOAL_EVENTS * (i + 1);
            if is_goal {
                let (root, thread) = &goal_roots[rng.below(goal_roots.len())];
                if starts[root] > day || day > starts[root] + 12 {
                    continue;
                }
                let phase = if rng.below(10) == 0 {
                    ThreadGoalPhase::Working
                } else {
                    ThreadGoalPhase::Complete
                };
                let body =
                    serde_json::to_string(&goal(&format!("goal-{root}"), thread, root, phase))?;
                insert_event(
                    &mut insert,
                    at,
                    "Orchestrator",
                    "ThreadGoalUpdated",
                    root,
                    &body,
                )?;
                last_event.insert(root.clone(), at);
                goal_n += 1;
                i += 1;
                continue;
            }
            let (run, _, _) = &runs[rng.below(runs.len())];
            if eventless.contains(run) || starts[run] > day || day > starts[run] + 12 {
                continue;
            }
            let mut pick = rng.below(1000) as u32;
            let mut chosen = &MIX[10];
            for entry in &MIX {
                if pick < entry.2 {
                    chosen = entry;
                    break;
                }
                pick -= entry.2;
            }
            let body = if chosen.1 == "AgentRunStateChanged" {
                serde_json::to_string(&state_change(
                    run,
                    AgentRunPhase::Pending,
                    AgentRunPhase::Running,
                ))?
            } else {
                format!(
                    "{{\"kind\":\"{}\",\"payload\":{{\"kind\":\"{}\",\"payload\":{{\"pad\":\"{}\"}}}}}}",
                    chosen.0,
                    chosen.1,
                    "x".repeat(chosen.3)
                )
            };
            insert_event(&mut insert, at, chosen.0, chosen.1, run, &body)?;
            last_event.insert(run.clone(), at);
            i += 1;
        }
    }
    {
        let mut rc = tx.prepare("INSERT INTO host_input_receipts(prompt_id,run_id,root_run_id,text,status,accepted_at_ns,status_at_ns) VALUES(?1,?2,?2,'t','delivered',?3,?3)")?;
        let mut cs = tx.prepare("INSERT INTO host_chat_submissions(submission_id,fingerprint,run_id,provenance_json,accepted_at_ns) VALUES(?1,'f',?2,'{}',?3)")?;
        for i in 0..3_000 {
            let (run, _, _) = &runs[rng.below(runs.len())];
            let at = now - (31 + rng.below(80) as i64) * DAY_NS;
            rc.execute(params![format!("p{i}"), run, at])?;
            cs.execute(params![format!("s{i}"), run, at])?;
        }
        let mut uq = tx.prepare("INSERT INTO user_questions(id,run_id,root_run_id,root_name,payload,answered) VALUES(?1,?2,?3,'r','{}',?4)")?;
        for i in 0..300 {
            let (run, _, root) = &runs[BIG_RUNS + rng.below(runs.len() - BIG_RUNS)];
            uq.execute(params![format!("q{i}"), run, root, i64::from(i % 60 != 0)])?;
        }
    }
    let mut set_msgs = tx.prepare("UPDATE run_contexts SET message_count=3 WHERE run_id=?1")?;
    for run in &eventless {
        set_msgs.execute([run])?;
    }
    drop(set_msgs);
    // Large payloads as the newest row of a few family runs, and elsewhere.
    let big = format!(
        "{{\"kind\":\"Tool\",\"payload\":{{\"kind\":\"ToolCompleted\",\"payload\":{{\"pad\":\"{}\"}}}}}}",
        "y".repeat(256 * 1024)
    );
    let mut last_ids = vec![];
    for run in runs
        .iter()
        .take(BIG_RUNS)
        .map(|r| &r.0)
        .filter(|r| !eventless.contains(r))
        .take(8)
    {
        let at = last_event.get(run).copied().unwrap_or(now - 100 * DAY_NS);
        tx.execute(
            "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,1,?1,'Tool',?2,'ToolCompleted',?3)",
            params![at, big, run],
        )?;
        last_ids.push((run.clone(), tx.last_insert_rowid()));
    }
    for _ in 0..32 {
        let (run, _, _) = &runs[BIG_RUNS + rng.below(runs.len() - BIG_RUNS)];
        let at = last_event.get(run).copied().unwrap_or(now - 100 * DAY_NS);
        tx.execute(
            "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,1,?1,'Tool',?2,'ToolCompleted',?3)",
            params![at, big, run],
        )?;
    }
    tx.commit()?;
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    let _ = ends;
    Ok(Fixture {
        big_threads: (0..BIG_THREADS).map(|t| format!("thread-{t}")).collect(),
        big_runs: runs.iter().take(BIG_RUNS).map(|r| r.0.clone()).collect(),
        last_ids,
    })
}

struct Counters {
    rows_q: usize,
    bytes: usize,
    steps: i64,
    fullscan: i64,
    statements: usize,
    outcome: String,
}

// Same text as `migrations::backfill::backfill_pending`, run first by every decision.
const BACKFILL_PENDING: &str = "SELECT EXISTS(SELECT 1 FROM events WHERE subkind IS NULL) OR EXISTS(SELECT 1 FROM events WHERE run_id IS NULL AND subkind IN ('TaskProgressed', 'TaskCheckpoint', 'ThreadGoalUpdated'))";

fn drain_json(
    conn: &Connection,
    text: &'static str,
    args: impl rusqlite::Params,
    c: &mut Counters,
) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(text)?;
    stmt.reset_status(StatementStatus::VmStep);
    stmt.reset_status(StatementStatus::FullscanStep);
    let out = stmt
        .query_map(args, |r| r.get::<_, rusqlite::types::Value>(0))?
        .map(|v| {
            Ok(match v? {
                rusqlite::types::Value::Text(s) => s,
                other => format!("{other:?}"),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    c.steps += i64::from(stmt.get_status(StatementStatus::VmStep));
    c.fullscan += i64::from(stmt.get_status(StatementStatus::FullscanStep));
    c.statements += 1;
    Ok(out)
}

/// One decision for the family, in the order the production tick uses.
fn decide(conn: &Connection, threads: &[String], runs: &[String], cutoff: i64) -> Result<Counters> {
    let mut c = Counters {
        rows_q: 0,
        bytes: 0,
        steps: 0,
        fullscan: 0,
        statements: 0,
        outcome: "Due".into(),
    };
    let run_json = serde_json::to_string(runs)?;
    let thread_json = serde_json::to_string(threads)?;
    drain_json(
        conn,
        sql::THREAD_RUNS,
        params![thread_json, 1_024i64],
        &mut c,
    )?;
    let _: bool = conn.query_row(BACKFILL_PENDING, [], |r| r.get(0))?;
    c.statements += 1;
    let recent = drain_json(conn, sql::RECENT_RUNS, params![run_json, cutoff], &mut c)?;
    for run in &recent {
        let mut stmt = conn.prepare_cached(sql::RUN_TAIL)?;
        stmt.reset_status(StatementStatus::VmStep);
        let mut rows = stmt.query(params![run, (P + 1) as i64])?;
        let mut seen = 0;
        let mut hold = None;
        while let Some(row) = rows.next()? {
            seen += 1;
            c.rows_q += 1;
            let (kind, sub, at): (String, Option<String>, i64) =
                (row.get(1)?, row.get(2)?, row.get(3)?);
            let bytes: i64 = row.get(5)?;
            if at <= cutoff {
                break;
            }
            match (kind.as_str(), sub.as_deref()) {
                ("Message", Some("MessageCompleted" | "FinalResultPublished" | "MessagePart"))
                | ("Lifecycle", Some("TurnCompleted"))
                | ("AgentMessage", Some("Delivered")) => {
                    hold = Some("RecentActivity");
                    break;
                }
                ("Lifecycle", Some("AgentRunStateChanged")) => {
                    c.bytes += bytes as usize;
                    hold = Some("RecentActivity?");
                    break;
                }
                _ => {}
            }
            if seen > P || c.rows_q > Q {
                hold = Some("ActivityProbeBudget");
                break;
            }
        }
        drop(rows);
        c.steps += i64::from(stmt.get_status(StatementStatus::VmStep));
        c.statements += 1;
        if let Some(h) = hold {
            c.outcome = h.into();
            return Ok(c);
        }
    }
    if !drain_json(
        conn,
        sql::RECENT_RECEIPTS,
        params![run_json, cutoff],
        &mut c,
    )?
    .is_empty()
    {
        c.outcome = "RecentInput".into();
        return Ok(c);
    }
    if !drain_json(
        conn,
        sql::LEGACY_FALLBACK,
        params![run_json, cutoff],
        &mut c,
    )?
    .is_empty()
    {
        c.outcome = "LegacyActivityFallback".into();
        return Ok(c);
    }
    let goals = drain_goal_ids(conn, &run_json, &mut c)?;
    if goals.len() > K {
        c.outcome = "GoalDecodeBudget".into();
        return Ok(c);
    }
    for id in goals {
        let (ty, len): (String, i64) =
            conn.query_row(sql::EVENT_SHAPE, [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
        c.statements += 1;
        c.bytes += len as usize;
        if c.bytes > BYTES {
            c.outcome = "GoalDecodeBudget".into();
            return Ok(c);
        }
        let body: String = conn.query_row(sql::EVENT_BODY, [id], |r| r.get(0))?;
        c.statements += 1;
        let _decoded: EventKind = serde_json::from_str(&body)?;
        let _ = ty;
    }
    // Handoff receipt per candidate goal root (the family roots with a goal).
    for run in runs.iter().take(BIG_THREADS) {
        let mut stmt = conn.prepare_cached(sql::HANDOFF_SHAPE)?;
        let _ = stmt.query_map([run], |r| r.get::<_, i64>(0))?.count();
        c.statements += 1;
    }
    for text in [sql::QUESTIONS, sql::EVAL_SAMPLES] {
        let arg = if text == sql::EVAL_SAMPLES {
            &thread_json
        } else {
            &run_json
        };
        if !drain_json(conn, text, params![arg], &mut c)?.is_empty() {
            c.outcome = "Protected".into();
            return Ok(c);
        }
    }
    let mut stmt = conn.prepare_cached(sql::LANES)?;
    let _ = stmt
        .query_map([&thread_json], |r| r.get::<_, String>(0))?
        .count();
    c.statements += 1;
    Ok(c)
}

fn drain_goal_ids(conn: &Connection, run_json: &str, c: &mut Counters) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(sql::GOAL_LATEST)?;
    stmt.reset_status(StatementStatus::VmStep);
    stmt.reset_status(StatementStatus::FullscanStep);
    let ids = stmt
        .query_map([run_json], |r| r.get::<_, Option<i64>>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect();
    c.steps += i64::from(stmt.get_status(StatementStatus::VmStep));
    c.fullscan += i64::from(stmt.get_status(StatementStatus::FullscanStep));
    c.statements += 1;
    Ok(ids)
}

fn plans(conn: &Connection) -> Result<()> {
    let probes: [(&str, &str); 14] = [
        ("THREAD_RUNS", sql::THREAD_RUNS),
        ("BACKFILL_PENDING", BACKFILL_PENDING),
        ("RECENT_RUNS", sql::RECENT_RUNS),
        ("RUN_TAIL", sql::RUN_TAIL),
        ("GOAL_LATEST", sql::GOAL_LATEST),
        ("EVENT_SHAPE", sql::EVENT_SHAPE),
        ("RECENT_RECEIPTS", sql::RECENT_RECEIPTS),
        ("LEGACY_FALLBACK", sql::LEGACY_FALLBACK),
        ("HANDOFF_SHAPE", sql::HANDOFF_SHAPE),
        ("QUESTIONS", sql::QUESTIONS),
        ("EVAL_SAMPLES", sql::EVAL_SAMPLES),
        ("LANES", sql::LANES),
        ("EVENT_BODY", sql::EVENT_BODY),
        ("HANDOFF_BODY", sql::HANDOFF_BODY),
    ];
    for (name, text) in probes {
        let mut stmt = conn.prepare(&format!("EXPLAIN QUERY PLAN {text}"))?;
        let nulls = vec![rusqlite::types::Value::Null; stmt.parameter_count()];
        let lines: Vec<String> = stmt
            .query_map(rusqlite::params_from_iter(nulls), |r| r.get::<_, String>(3))?
            .collect::<std::result::Result<_, _>>()?;
        println!("plan {name}:");
        for line in lines {
            println!("    {line}");
        }
    }
    Ok(())
}

fn scenario(path: &PathBuf, name: &str, fx: &Fixture, trials: usize, cutoff: i64) -> Result<()> {
    let mut totals = vec![];
    let mut last = None;
    for _ in 0..trials {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        let started = Instant::now();
        conn.execute_batch("BEGIN")?;
        let c = decide(&conn, &fx.big_threads, &fx.big_runs, cutoff)?;
        conn.execute_batch("COMMIT")?;
        totals.push(started.elapsed().as_secs_f64() * 1000.0);
        last = Some(c);
    }
    let c = last.unwrap();
    let first = totals[0];
    println!(
        "scenario {name}: outcome={} stmts={} tail_rows={} decoded_bytes={} vm_steps={} fullscan_steps={} | n={trials} ms first={:.2} p50={:.2} p95={:.2} max={:.2}",
        c.outcome,
        c.statements,
        c.rows_q,
        c.bytes,
        c.steps,
        c.fullscan,
        first,
        pct(&mut totals.clone(), 0.5),
        pct(&mut totals.clone(), 0.95),
        pct(&mut totals, 1.0)
    );
    Ok(())
}

fn append_cost(path: &PathBuf, trials: usize) -> Result<()> {
    let conn = Connection::open(path)?;
    let goal_body = serde_json::to_string(&goal(
        "g-append",
        "thread-5",
        "run-5",
        ThreadGoalPhase::Working,
    ))?;
    let other_body = "{\"kind\":\"Tool\",\"payload\":{\"kind\":\"ToolCompleted\",\"payload\":{\"pad\":\"zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz\"}}}";
    let batch = |is_goal: bool| -> Result<f64> {
        let tx = conn.unchecked_transaction()?;
        let started = Instant::now();
        {
            let mut ins = tx.prepare(
                "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,1,1,?1,?2,?3,'run-5')",
            )?;
            for _ in 0..2_000 {
                if is_goal {
                    ins.execute(params!["Orchestrator", goal_body, "ThreadGoalUpdated"])?;
                } else {
                    ins.execute(params!["Tool", other_body, "ToolCompleted"])?;
                }
            }
        }
        let micros = started.elapsed().as_secs_f64() * 1e6 / 2_000.0;
        tx.rollback()?;
        Ok(micros)
    };
    let mut build = vec![];
    let mut rows: [(&str, Vec<f64>); 4] = [
        ("goal event, without index", vec![]),
        ("goal event, with index", vec![]),
        ("other event, without index", vec![]),
        ("other event, with index", vec![]),
    ];
    for _ in 0..trials {
        conn.execute_batch("DROP INDEX IF EXISTS idx_events_goal_by_run")?;
        rows[0].1.push(batch(true)?);
        rows[2].1.push(batch(false)?);
        let started = Instant::now();
        conn.execute_batch(sql::INDEX_DDL[0])?;
        build.push(started.elapsed().as_secs_f64());
        rows[1].1.push(batch(true)?);
        rows[3].1.push(batch(false)?);
    }
    println!(
        "goal partial index build over the whole events table (s): min={:.2} p50={:.2} max={:.2} (n={trials})",
        pct(&mut build.clone(), 0.0),
        pct(&mut build.clone(), 0.5),
        pct(&mut build, 1.0)
    );
    for (label, mut v) in rows {
        let mean = v.iter().sum::<f64>() / v.len() as f64;
        println!(
            "append {label}: us/insert mean={mean:.1} p50={:.1} p95={:.1} (n={})",
            pct(&mut v.clone(), 0.5),
            pct(&mut v, 0.95),
            v.len()
        );
    }
    Ok(())
}

fn main() -> Result<()> {
    let dir = PathBuf::from(std::env::args().nth(1).ok_or("usage: <work-dir>")?);
    std::fs::create_dir_all(&dir)?;
    let trials: usize = std::env::var("RDM_TRIALS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(30);
    let path = dir.join("decision-measure.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let mut config = StorageConfig {
        db_path: path.clone(),
        ..Default::default()
    };
    config.thread_index.enabled = false;
    let started = Instant::now();
    let fx = build(&config)?;
    let conn = Connection::open(&path)?;
    let events: i64 = conn.query_row("SELECT count(*) FROM events", [], |r| r.get(0))?;
    let goals: i64 = conn.query_row(
        "SELECT count(*) FROM events WHERE subkind='ThreadGoalUpdated'",
        [],
        |r| r.get(0),
    )?;
    let version: String = conn.query_row("SELECT sqlite_version()", [], |r| r.get(0))?;
    println!(
        "fixture: {events} events ({goals} goal rows), {THREADS} threads, family of {BIG_THREADS} threads/{BIG_RUNS} runs, sqlite {version}, db {} MiB, built in {:.0}s; disk: nvme (df), os cache warm, per-trial fresh connection (cold sqlite page cache)",
        std::fs::metadata(&path)?.len() >> 20,
        started.elapsed().as_secs_f64()
    );
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")?;
    plans(&conn)?;
    let now = now_ns();
    let cutoff = now - 7 * DAY_NS;
    scenario(&path, "abandoned (all 624 runs stale)", &fx, trials, cutoff)?;
    // active: a recent completed message on one run.
    conn.execute(
        "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,1,?1,'Message','{}','MessageCompleted',?2)",
        params![now - DAY_NS, fx.big_runs[600]],
    )?;
    let active_id = conn.last_insert_rowid();
    scenario(
        &path,
        "active (recent message, run 600)",
        &fx,
        trials,
        cutoff,
    )?;
    conn.execute("DELETE FROM events WHERE id=?1", [active_id])?;
    // recent non-activity rows: 8 per run on 30 runs, then on 200 runs.
    for (label, count) in [
        ("recent tool noise (30 runs x 8 rows)", 30usize),
        ("recent tool noise (200 runs x 8 rows)", 200),
    ] {
        let mut ids = vec![];
        for run in fx.big_runs.iter().take(count) {
            for _ in 0..8 {
                conn.execute(
                    "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) VALUES('s',1,1,?1,'Tool','{}','ToolCompleted',?2)",
                    params![now - DAY_NS, run],
                )?;
                ids.push(conn.last_insert_rowid());
            }
        }
        scenario(&path, label, &fx, trials, cutoff)?;
        for id in ids {
            conn.execute("DELETE FROM events WHERE id=?1", [id])?;
        }
    }
    // recent save of an event-less conversation run.
    conn.execute(
        "UPDATE run_contexts SET updated_at_ns=?1 WHERE run_id='run-7'",
        [now - DAY_NS],
    )?;
    scenario(
        &path,
        "legacy fallback (event-less run saved recently)",
        &fx,
        trials,
        cutoff,
    )?;
    conn.execute(
        "UPDATE run_contexts SET updated_at_ns=?1 WHERE run_id='run-7'",
        [now - 60 * DAY_NS],
    )?;
    // newest row of a run is a 256 KiB payload (scenario already includes 8 such family runs).
    println!(
        "256 KiB newest rows in the family: {} (included in every scenario above)",
        fx.last_ids.len()
    );
    append_cost(&path, trials.max(20))?;
    // Doubling unrelated rows must not change VM steps of the stale scenario.
    let c0 = decide(
        &Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?,
        &fx.big_threads,
        &fx.big_runs,
        cutoff,
    )?;
    conn.execute(
        "INSERT INTO events(session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,run_id) \
         SELECT session_id,schema_version,monotonic_ns,wall_clock_ns,kind,payload,subkind,'run-unrelated' FROM events WHERE run_id NOT IN (SELECT value FROM json_each(?1)) AND subkind IS NOT NULL",
        [serde_json::to_string(&fx.big_runs)?],
    )?;
    let c1 = decide(
        &Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY)?,
        &fx.big_threads,
        &fx.big_runs,
        cutoff,
    )?;
    println!(
        "vm steps with unrelated events+goals doubled: before={} after={} (statements {} / {})",
        c0.steps, c1.steps, c0.statements, c1.statements
    );
    Ok(())
}
