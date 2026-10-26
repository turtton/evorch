//! S8: real agent-loop thread_search benchmark. See docs/thread-search-benchmark.md.
//! No tracing subscriber is installed: source/query/result text must not be logged.

#[path = "thread_search_measure/copy.rs"]
mod copy;
#[path = "thread_search_measure/corpus.rs"]
mod corpus;
#[path = "thread_search_measure/diagnose.rs"]
mod diagnose;
#[path = "thread_search_measure/driver.rs"]
mod driver;
#[path = "thread_search_measure/job.rs"]
mod job;
#[path = "thread_search_measure/manifest.rs"]
mod manifest;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use storage::{Database, Storage, StorageConfig};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REPETITIONS: usize = 100;
const WARMUPS: usize = 5;
const PAGE_LIMIT: usize = 20;
const LATENCY_REFERENCE_US: u64 = 300_000;
const DEADLINE: Duration = Duration::from_secs(120);
const BACKUP: &str = "/var/tmp/evorch-scratch-1000-1b531f2bc252cbe8/t-thread-199/w6-measure-20261025T215631/store-copy.db";

fn require(ok: bool, code: &'static str) -> Result<()> {
    if ok { Ok(()) } else { Err(code.into()) }
}

fn error_class(error: &dyn std::error::Error) -> String {
    let text = error.to_string();
    if text.len() <= 80
        && text
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        text
    } else {
        "external_error".into()
    }
}
fn config(path: &Path) -> StorageConfig {
    StorageConfig {
        db_path: path.to_owned(),
        ..Default::default()
    }
}

struct Options {
    work: PathBuf,
    project: PathBuf,
    prepared: Option<PathBuf>,
    cell: Option<String>,
    import: Option<PathBuf>,
    aggregate: bool,
    diagnose: bool,
    diagnose_class: Option<String>,
}

fn options() -> Result<Options> {
    let mut args = std::env::args_os().skip(1);
    let work = args
        .next()
        .ok_or("usage: thread_search_measure <NEW-work-dir> <project-root>")?;
    let project = args.next().ok_or("project_root_required")?;
    let mut prepared = None;
    let mut cell = None;
    let mut import = None;
    let mut aggregate = false;
    let mut diagnose = false;
    let mut diagnose_class = None;
    while let Some(flag) = args.next() {
        match flag.to_str() {
            Some("--prepared-seed") => {
                prepared = Some(PathBuf::from(args.next().ok_or("missing_seed")?))
            }
            Some("--cell") => {
                cell = Some(
                    args.next()
                        .ok_or("missing_cell")?
                        .to_str()
                        .ok_or("non_utf8_cell")?
                        .to_owned(),
                )
            }
            Some("--import-summary") => {
                import = Some(PathBuf::from(args.next().ok_or("missing_import")?))
            }
            Some("--aggregate") => aggregate = true,
            Some("--diagnose") => diagnose = true,
            Some("--diagnose-class") => {
                diagnose = true;
                diagnose_class = Some(
                    args.next()
                        .ok_or("missing_diagnostic_class")?
                        .to_str()
                        .ok_or("non_utf8_diagnostic_class")?
                        .to_owned(),
                );
            }
            _ => return Err("unexpected_argument".into()),
        }
    }
    let project = PathBuf::from(project).canonicalize()?;
    let work = PathBuf::from(work);
    if let Some(seed) = &prepared {
        let seed = seed.canonicalize()?;
        let approved = Path::new(BACKUP)
            .parent()
            .and_then(Path::parent)
            .ok_or("backup_parent_missing")?;
        require(
            seed.starts_with(approved)
                && seed.file_name().is_some_and(|name| name == "seed.db")
                && seed != Path::new(BACKUP),
            "prepared_seed_outside_approved_scratch",
        )?;
    }
    require(
        !work.exists() || prepared.is_some(),
        "existing_work_requires_prepared_seed",
    )?;
    Ok(Options {
        work,
        project,
        prepared,
        cell,
        import,
        aggregate,
        diagnose,
        diagnose_class,
    })
}

fn percentile(values: &[u64], percent: usize) -> u64 {
    assert!(!values.is_empty() && (1..=100).contains(&percent));
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[(percent * sorted.len()).div_ceil(100) - 1]
}

fn timing(values: &[u64]) -> Value {
    json!({
        "count": values.len(), "p50_us": percentile(values, 50),
        "p95_us": percentile(values, 95), "max_us": values.iter().max(),
    })
}

fn pressure() -> Value {
    // These are system counters, not process arguments, paths or source text.
    let read = |path| std::fs::read_to_string(path).ok();
    json!({
        "loadavg": read("/proc/loadavg"),
        "cpu": read("/proc/pressure/cpu"),
        "io": read("/proc/pressure/io"),
        "memory": read("/proc/pressure/memory"),
    })
}

async fn cell(
    seed: &Path,
    path: &Path,
    project: &Path,
    corpus: &corpus::Corpus,
    case: &corpus::Case,
    cold: bool,
    backfill: bool,
) -> Result<Value> {
    copy::backup(seed, path)?;
    let cfg = config(path);
    let storage = Storage::open(cfg.clone())?;
    let handle = storage.handle();
    let before = job::observation(&handle, path)?;
    let load_before = pressure();
    let expected = corpus.expected(case)?;
    require(
        case.class == "nohit" || !expected.is_empty(),
        "empty_positive_oracle",
    )?;
    let mut pages = Vec::new();
    let mut first_pages = Vec::new();
    let mut drains = Vec::new();
    let mut partial_pages = 0;
    let mut max_bytes = 0;
    let mut output_hashes = BTreeMap::<String, usize>::new();
    let mut progress = Vec::new();
    let mut warm = None;
    if !cold {
        warm = Some(driver::Harness::new(&cfg, handle.clone())?);
        for _ in 0..WARMUPS {
            warm.as_ref()
                .unwrap()
                .search(project, case, &expected)
                .await?;
        }
    }
    let mut worker = if backfill {
        Some(job::Parallel::new(handle.clone(), &corpus.other_projects)?)
    } else {
        None
    };
    for repetition in 0..REPETITIONS {
        let fresh;
        let harness = if cold {
            fresh = driver::Harness::new(&cfg, handle.clone())?;
            &fresh
        } else {
            warm.as_ref().unwrap()
        };
        // The worker waits for this exact search's first ToolUse. No index step
        // or progress-barrier wait is inserted into the timed model response.
        if let Some(worker) = &mut worker {
            harness.arm_backfill(worker.arm()?);
        }
        let sample = harness.search(project, case, &expected).await?;
        if let Some(worker) = &mut worker {
            progress.push(worker.finish(repetition, &sample)?);
        }
        first_pages.push(sample.pages_us[0]);
        pages.extend(sample.pages_us);
        drains.push(sample.drain_us);
        partial_pages += sample.partial_pages;
        max_bytes = max_bytes.max(sample.max_bytes);
        *output_hashes.entry(sample.hash).or_default() += 1;
        if repetition % 10 == 9 {
            println!(
                "{}",
                json!({
                    "stage": "progress", "class": case.class, "cold": cold,
                    "backfill": backfill, "completed_drains": repetition + 1,
                    "pages": pages.len(),
                })
            );
        }
    }
    if let Some(worker) = worker {
        worker.close()?;
    }
    let after = job::observation(&handle, path)?;
    Ok(json!({
        "status": "passed", "latency_policy": "advisory",
        "class": case.class, "query_sha256": case.hash(),
        "cache": if cold {"new_sqlite_connection_not_os_cold"} else {"same_connection_warm"},
        "backfill": backfill, "repetitions": REPETITIONS,
        "warmup_drains": if cold {0} else {WARMUPS},
        "first_page": timing(&first_pages), "all_pages": timing(&pages),
        "full_drain": timing(&drains),
        "page_p95_lt_300ms": percentile(&pages, 95) < LATENCY_REFERENCE_US,
        "first_page_p95_lt_300ms": percentile(&first_pages, 95) < LATENCY_REFERENCE_US,
        "canonical_threads": expected.len(), "canonical_hits": expected.values().sum::<u64>(),
        "canonical_sha256": corpus::set_hash(&expected),
        "partial_pages": partial_pages, "max_output_bytes": max_bytes,
        "output_hash_counts": output_hashes, "backfill_progress": progress,
        "before": before, "after": after,
        "pressure_before": load_before, "pressure_after": pressure(),
    }))
}

async fn run() -> Result<()> {
    require(!cfg!(debug_assertions), "release_build_required")?;
    let options = options()?;
    let work = &options.work;
    let project = &options.project;
    std::fs::create_dir_all(work)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(work, std::fs::Permissions::from_mode(0o700))?;
        let stat = rustix::fs::statvfs(work)?;
        let available = u128::from(stat.f_bavail) * u128::from(stat.f_frsize);
        let copies = if options.cell.is_some() { 2 } else { 29 };
        let minimum = u128::from(std::fs::metadata(BACKUP)?.len()) * copies + (16_u128 << 30);
        require(
            options.aggregate || available >= minimum,
            "insufficient_copy_capacity",
        )?;
    }
    let seed = options
        .prepared
        .clone()
        .unwrap_or_else(|| work.join("seed.db"));
    if options.prepared.is_none() {
        copy::backup(Path::new(BACKUP), &seed)?;
    }
    let cfg = config(&seed);
    // A prepared seed is only ever read through read-only diagnostic/backup
    // connections. Database/Storage open only our new seed or independent probe.
    let probe = if options.prepared.is_some() {
        let directory = tempfile::Builder::new()
            .prefix("s8-probe-")
            .tempdir_in(work)?;
        copy::backup(&seed, &directory.path().join("probe.db"))?;
        Some(directory)
    } else {
        None
    };
    let reading = probe
        .as_ref()
        .map(|dir| config(&dir.path().join("probe.db")))
        .unwrap_or_else(|| cfg.clone());
    let database = Database::open(&reading)?;
    let corpus = corpus::Corpus::load(&database, &reading.db_path, project)?;
    println!(
        "{}",
        json!({
            "stage": "preflight", "classification": corpus.classification,
            "scope_sha256": corpus::hash(project.to_string_lossy().as_bytes()),
            "visible_threads": corpus.threads.len(), "other_project_count": corpus.other_projects.len(),
            "other_project_indexable": corpus.other_indexable,
            "source_bytes": std::fs::metadata(BACKUP)?.len(), "plan_cells": 28,
            "repetitions_per_cell": REPETITIONS, "warmups_per_warm_cell": WARMUPS,
        })
    );
    let started = Instant::now();
    if options.prepared.is_none() {
        let storage = Storage::open(cfg.clone())?;
        job::preindex(&storage.handle(), &database, &corpus)?;
        drop(storage);
    } else {
        corpus.verify_seed(&database)?;
    }
    let cases = corpus.cases()?;
    if options.diagnose {
        let case = cases
            .iter()
            .find(|case| {
                options
                    .diagnose_class
                    .as_deref()
                    .is_none_or(|class| case.class == class)
            })
            .ok_or("unknown_diagnostic_class")?;
        let value = diagnose::reads(&database, &reading.db_path, &corpus)?;
        println!("{value}");
        let diagnostic_database = database;
        let diagnostic_probe = probe;
        let path = work.join("diagnostic-cell.db");
        copy::backup(&seed, &path)?;
        let cfg = config(&path);
        let storage = Storage::open(cfg.clone())?;
        let harness = driver::Harness::new(&cfg, storage.handle())?;
        let expected = corpus.expected(case)?;
        let sample = harness.search(project, case, &expected).await?;
        require(
            sample.boundary_us.len() == sample.pages_us.len(),
            "diagnostic_lifecycle_timestamp_missing",
        )?;
        println!(
            "{}",
            json!({"stage":"diagnostic_loop","pages_us":sample.pages_us,
            "before_dispatch_dispatch_after_us":sample.boundary_us,
            "class":case.class,"query_sha256":case.hash(),
            "canonical_threads":expected.len(),"canonical_hits":expected.values().sum::<u64>(),
            "canonical_sha256":corpus::set_hash(&expected),
            "drain_us":sample.drain_us,"page_shapes":sample.page_shapes,"not_acceptance_measurement":true})
        );
        println!(
            "{}",
            diagnose::candidates(
                &diagnostic_database,
                &reading.db_path,
                &corpus,
                case,
                &sample
            )?
        );
        drop(diagnostic_probe);
        return Ok(());
    }
    drop(database);
    drop(probe);
    let mut manifest = manifest::Manifest::open(work, &seed, project, &corpus, &cases)?;
    if let Some(source) = &options.import {
        manifest.import(source)?;
    }
    if options.aggregate {
        let summary = manifest.summary();
        println!("{summary}");
        return require(summary["s8_pass"] == true, "s8_incomplete_or_failed");
    }
    let mut selected = 0;
    for backfill in [false, true] {
        for cold in [false, true] {
            for case in &cases {
                let id = manifest::cell_id(case, cold, backfill);
                if options.cell.as_ref().is_some_and(|wanted| *wanted != id) {
                    continue;
                }
                selected += 1;
                if manifest.done(&id) {
                    require(manifest.passed(&id), "recorded_cell_failed")?;
                    continue;
                }
                let path = work.join(format!("{id}.db"));
                let value = match cell(&seed, &path, project, &corpus, case, cold, backfill).await {
                    Ok(value) => value,
                    Err(error) => json!({
                        "class":case.class, "cold":cold, "backfill":backfill, "status":"failed",
                        "error_class":error_class(error.as_ref()),
                        "error_sha256":corpus::hash(error.to_string().as_bytes()),
                    }),
                };
                manifest.record(&id, value.clone())?;
                println!(
                    "{}",
                    json!({"stage":"cell", "cell_id":id,
                    "conditions_sha256":manifest.conditions, "result":value})
                );
                // Only blocking validation/resource failures stop the matrix.
                // Slow validated cells remain complete and report their timing.
                if !manifest.passed(&id) {
                    println!("{}", manifest.summary());
                    return Err("s8_cell_failed".into());
                }
            }
        }
    }
    require(selected > 0, "unknown_cell")?;
    let summary = manifest.summary();
    println!(
        "{}",
        json!({"stage":"elapsed", "seconds":started.elapsed().as_secs()})
    );
    println!("{summary}");
    if options.cell.is_some() {
        Ok(())
    } else {
        require(summary["s8_pass"] == true, "s8_incomplete_or_failed")
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    // A panic's payload can contain data-dependent library diagnostics.
    std::panic::set_hook(Box::new(|_| {
        eprintln!(r#"{{"stage":"panic","status":"failed"}}"#)
    }));
    if let Err(error) = run().await {
        eprintln!(
            "{}",
            json!({
                "stage": "fatal", "error_sha256": corpus::hash(error.to_string().as_bytes()),
            "error_class": error_class(error.as_ref()),
                "status": "failed",
            })
        );
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{ContentBlock, Message, Role as MessageRole};
    use storage::thread_index::{
        BlockOrigin, BlockProvenance, MessageProvenance, ProvenanceEvidence, ProvenanceMap,
        ProvenanceStatus, SnapshotWrite,
    };
    use storage::threads::{ThreadRunRow, ThreadUpsert};

    #[test]
    fn nearest_rank_boundaries() {
        assert_eq!(super::percentile(&(1..=100).collect::<Vec<_>>(), 95), 95);
        assert_eq!(super::percentile(&[7], 95), 7);
        assert_eq!(super::percentile(&[2, 1], 95), 2);
    }

    #[test]
    fn backup_is_independent_and_refuses_existing_target() {
        let temp = tempfile::tempdir().unwrap();
        let source = temp.path().join("source.db");
        let target = temp.path().join("target.db");
        {
            let conn = rusqlite::Connection::open(&source).unwrap();
            conn.execute_batch(
                "CREATE TABLE fixture(value INTEGER); INSERT INTO fixture VALUES(7)",
            )
            .unwrap();
        }
        let bytes = std::fs::read(&source).unwrap();
        copy::backup(&source, &target).unwrap();
        assert!(copy::backup(&source, &target).is_err());
        let conn = rusqlite::Connection::open(&target).unwrap();
        conn.execute("UPDATE fixture SET value=9", []).unwrap();
        assert_eq!(std::fs::read(&source).unwrap(), bytes);
    }

    fn native(
        handle: &storage::StorageHandle,
        thread: &str,
        run: &str,
        project: &Path,
        messages: Vec<Message>,
    ) {
        handle
            .upsert_thread_runtime(&ThreadUpsert {
                thread_id: thread.into(),
                project_root: Some(project.to_str().unwrap().into()),
                root_run_id: Some(run.into()),
                title: Some("fixture title".into()),
                created_at_ns: 1,
                updated_at_ns: 1,
                ..Default::default()
            })
            .unwrap();
        // These are newly authored synthetic bytes, not promotion of legacy
        // data. The production shared writer validates the complete map.
        let map = ProvenanceMap {
            version: 1,
            raw_message_count: messages.len(),
            checkpoint_count: 0,
            messages: messages
                .iter()
                .enumerate()
                .map(|(ordinal, message)| MessageProvenance {
                    raw_index: ordinal,
                    ordinal,
                    blocks: message
                        .content
                        .iter()
                        .enumerate()
                        .map(|(block_index, _)| BlockProvenance {
                            block_index,
                            origin: BlockOrigin::Native,
                            excluded_reason: None,
                        })
                        .collect(),
                })
                .collect(),
        };
        handle
            .save_snapshot_bundle(&SnapshotWrite {
                record: storage::RunContextRecord {
                    run_id: run.into(),
                    role: "Worker".into(),
                    name: format!("chat:Worker:{thread}"),
                    parent_run_id: None,
                    config_json: "{}".into(),
                    messages_json: serde_json::to_string(&messages).unwrap(),
                    checkpoints_json: "[]".into(),
                    terminal_phase: "Done".into(),
                    restorable: false,
                    updated_at_ns: 2,
                },
                expected_snapshot_revision: None,
                binding: Some(ThreadRunRow {
                    run_id: run.into(),
                    thread_id: thread.into(),
                    root_run_id: run.into(),
                    depth: 0,
                }),
                status: ProvenanceStatus::Verified {
                    evidence: ProvenanceEvidence::CompletePositionMap,
                },
                map,
            })
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn actual_dispatch_checks_authorization_metadata_index_and_cursor_drain() {
        let temp = tempfile::tempdir().unwrap();
        let project = temp.path().join("project");
        let other = temp.path().join("other");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(&other).unwrap();
        let cfg = config(&temp.path().join("fixture.db"));
        let storage = Storage::open(cfg.clone()).unwrap();
        let handle = storage.handle();
        let messages = |text: &str| {
            vec![Message {
                role: MessageRole::Assistant,
                content: vec![ContentBlock::Text { text: text.into() }],
            }]
        };
        for n in 1..=21 {
            native(
                &handle,
                &format!("thread-{n}"),
                &format!("run-{n}"),
                &project,
                messages("fixtureword 日本語 alpha beta"),
            );
        }
        native(
            &handle,
            "thread-1",
            "run-97",
            &project,
            messages("fixtureword earlierroot"),
        );
        native(
            &handle,
            "hidden",
            "run-99",
            &other,
            messages("fixtureword hiddenonly"),
        );
        native(
            &handle,
            "partial",
            "run-98",
            &project,
            vec![Message {
                role: MessageRole::Assistant,
                content: (0..2001)
                    .map(|_| ContentBlock::Text {
                        text: "fixtureword".into(),
                    })
                    .collect(),
            }],
        );
        let db = Database::open(&cfg).unwrap();
        let sources = (1..=21)
            .map(|n| format!("run-{n}"))
            .chain(["run-99".into(), "run-98".into(), "run-97".into()])
            .collect();
        job::preindex_project(&handle, &db, &sources, project.to_str().unwrap()).unwrap();
        job::preindex_project(&handle, &db, &sources, other.to_str().unwrap()).unwrap();
        assert_eq!(
            db.thread_index_run_status("run-99").unwrap().unwrap().state,
            storage::thread_index::IndexState::Indexed
        );
        let harness = driver::Harness::new(&cfg, handle).unwrap();
        let mut expected = (1..=21)
            .map(|n| (format!("thread-{n}"), 1))
            .collect::<BTreeMap<_, _>>();
        expected.insert("partial".into(), 2001);
        expected.insert("thread-1".into(), 2);
        let sample = harness
            .search(
                &project,
                &corpus::Case::fixture("fixtureword", "words"),
                &expected,
            )
            .await
            .unwrap();
        assert!(sample.pages_us.len() >= 2);
        assert!(sample.partial_pages > 0);
        assert!(sample.max_bytes <= 16_384);
        let expected = (1..=21)
            .map(|n| (format!("thread-{n}"), 1))
            .chain([("partial".into(), 1)])
            .collect();
        assert!(
            harness
                .search(
                    &project,
                    &corpus::Case::fixture("hiddenonly", "words"),
                    &BTreeMap::new()
                )
                .await
                .is_ok()
        );
        assert!(
            harness
                .search(
                    &project,
                    &corpus::Case::fixture("fixture title", "phrase"),
                    &expected
                )
                .await
                .is_ok()
        );
    }
}
