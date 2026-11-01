//! Runs the GUI start-up (exclusive phase, sidebar clean-up, background reclaim)
//! against copies of a real database, without a window. Never point it at the
//! originals: it deletes.
//!
//! ```text
//! cargo run -p gui --example startup_phase_check -- \
//!     --db COPY/evorch-events.db --root COPY/threads --sidebar COPY/sidebar.json \
//!     [--days 30] [--reclaim-cycles 20]
//! ```

use std::path::PathBuf;
use std::time::Instant;

use gui::startup::{StartupEvent, StartupPlan, run_startup};
use runtime::thread_retention::{ExecutorConfig, ReclaimExecutor};
use storage::thread_retention::{LegacyArchivedPolicy, ThreadRetentionPolicy};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut db = None;
    let mut root = None;
    let mut sidebar = None;
    let mut days = 30_u32;
    let mut reclaim_cycles = 0_u64;
    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        let mut value = || arguments.next().ok_or("missing value");
        match argument.as_str() {
            "--db" => db = Some(PathBuf::from(value()?)),
            "--root" => root = Some(PathBuf::from(value()?)),
            "--sidebar" => sidebar = Some(PathBuf::from(value()?)),
            "--days" => days = value()?.parse()?,
            "--reclaim-cycles" => reclaim_cycles = value()?.parse()?,
            other => return Err(format!("unknown option {other}").into()),
        }
    }
    let (db, root, sidebar) = (
        db.ok_or("--db")?,
        root.ok_or("--root")?,
        sidebar.ok_or("--sidebar")?,
    );
    let plan = StartupPlan {
        db_path: db.clone(),
        ownership_root: root,
        sidebar_path: Some(sidebar.clone()),
        artifacts_dir: db.parent().map(|parent| parent.join("artifacts")),
        run_phase: true,
    };
    let started = Instant::now();
    let policy = move |_: &workspace_ui::SidebarState| ThreadRetentionPolicy {
        archived_delete_after_days: days,
        legacy: LegacyArchivedPolicy::Immediate,
    };
    let mut prelude = run_startup(&plan, policy, &mut |event| {
        let at = started.elapsed().as_secs_f32();
        match event {
            StartupEvent::Phase(progress) => println!("[{at:7.2}s] {progress:?}"),
            other => println!("[{at:7.2}s] {other:?}"),
        }
    })?;
    println!("elapsed {:.2}s", started.elapsed().as_secs_f32());
    let confirmed = gui::app::confirmed_threads(&prelude.report);
    println!(
        "confirmed threads: {}, tombstones: {} threads / {} runs / {} tasks, binding: {:?}",
        confirmed.len(),
        prelude.tombstones.threads.len(),
        prelude.tombstones.runs.len(),
        prelude.tombstones.tasks.len(),
        prelude.binding
    );
    if let runtime::thread_retention::StartupOutcome::Ran { blocked, .. } = &prelude.report.outcome
    {
        for (family, reason) in blocked {
            println!("blocked {family}: {reason:?}");
        }
    }
    match gui::app::startup_notice(&prelude.report, prelude.binding.as_ref()) {
        Some((kind, summary)) => println!("notice {kind:?}: {summary}"),
        None => println!("notice: none"),
    }
    if let Some(mut state) = prelude.sidebar.take() {
        let before = state.threads.len();
        let removed = gui::app::prune_deleted(&mut state, &prelude.tombstones);
        workspace_ui::save_sidebar(&state, &sidebar)?;
        println!(
            "sidebar threads {before} -> {} ({removed} removed)",
            state.threads.len()
        );
    }
    if reclaim_cycles > 0 {
        let config = storage::StorageConfig {
            db_path: db,
            ..storage::StorageConfig::default()
        };
        let storage =
            storage::Storage::open_with_lease(config.clone(), prelude.leases.db_lease()?)?;
        // As the GUI does: without the store the executor leaves artifact files alone.
        let artifacts = plan
            .artifacts_dir
            .as_deref()
            .and_then(|dir| runtime::artifacts::ArtifactStore::new(dir).ok())
            .map(std::sync::Arc::new);
        let mut executor_config = ExecutorConfig::new(storage.handle(), config, artifacts);
        executor_config.interval = std::time::Duration::from_millis(10);
        let executor = ReclaimExecutor::spawn(executor_config)?;
        let report = executor.wait_for_cycles(reclaim_cycles);
        println!("reclaim after {} cycles: {report:?}", report.cycles);
        executor.shutdown();
        storage.close();
    }
    Ok(())
}
