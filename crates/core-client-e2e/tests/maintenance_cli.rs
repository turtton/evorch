//! `evorch maintenance backfill-events` (stage 17d-2) and `evorch reindex-threads`
//! (stage 17d-3) against a real core (ADR 0045): the CLI's own code (`evorch::online_maintenance`, the entry it shares
//! with `evorch eval`) over the Unix socket of a production maintenance area
//! over a real database.
//!
//! The CLI is a process of its own here (this test binary, started again with
//! `--exact` on the "child" test below), so what it prints and how it ends are
//! observed from outside, and an interruption is a real signal. The area is held
//! between two batches by a gate the test opens, so every wait is for an event
//! the core, the gate or the child sends, never for time to pass.
use std::io::{BufRead as _, BufReader, Read as _};
use std::os::unix::process::ExitStatusExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use evorch::cli::core_client::{client_config, connect_core, finish};
use evorch::online_maintenance::{self, BackfillRequest, ReindexEnd, backfill};
use serde_json::{Value, json};
use workbench_client::WorkbenchClient;
use workbench_host::areas::maintenance::{
    MaintenanceArea, MaintenanceHook, MaintenanceSeams, MaintenanceStage,
};
use workbench_host::areas::{AreaTasks, Areas};
use workbench_host::bootstrap::{BootstrapOptions, BootstrappedHost};
use workbench_host::connections::ConnectionHooks;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::handshake::Capability;
use workbench_protocol::ids::ClientId;
use workbench_protocol::namespaces::maintenance::{
    ListMaintenance, MaintenanceEvent, MaintenanceProgress, MaintenanceQuery, MaintenanceReply,
    MaintenanceView, ReindexResult, ReindexThreads,
};
use workbench_protocol::operation::{CancelReason, OperationState};
use workbench_protocol::queries::{Query, QueryReply};

struct Idle;

#[async_trait]
impl runtime::AgentModel for Idle {
    async fn complete(
        &self,
        _: &runtime::AgentInvocationContext,
        _: runtime::Role,
        _: &[providers::Message],
        _: &[providers::ToolSpec],
    ) -> Result<providers::ChatResponse, runtime::RuntimeError> {
        std::future::pending().await
    }

    fn selected_model(&self, _: runtime::Role, _: Option<&str>) -> String {
        "maintenance-cli".into()
    }
}

/// A gate the test opens: every operation that has committed `batches` batches
/// counts itself in `reached` at the next batch boundary and waits there.
struct Hold {
    batches: u32,
    reached: tokio::sync::Semaphore,
    open: tokio::sync::watch::Sender<bool>,
}

impl Hold {
    fn after(batches: u32) -> Arc<Self> {
        Arc::new(Self {
            batches,
            reached: tokio::sync::Semaphore::new(0),
            open: tokio::sync::watch::channel(false).0,
        })
    }

    fn hook(self: &Arc<Self>) -> MaintenanceHook {
        let hold = self.clone();
        Arc::new(move |stage, _, batches| {
            let hold = hold.clone();
            Box::pin(async move {
                if stage == MaintenanceStage::BeforeBatch && batches == hold.batches {
                    hold.reached.add_permits(1);
                    let _ = hold.open.subscribe().wait_for(|open| *open).await;
                }
            })
        })
    }

    async fn reached(&self) {
        self.reached.acquire().await.unwrap().forget();
    }

    fn release(&self) {
        self.open.send_replace(true);
    }
}

/// The launcher of the CLI: it meets the core at `socket` as the CLI meets the one of its configuration.
fn launcher(identity: &Path, socket: &Path) -> core_launcher::Launcher {
    core_launcher::Launcher::with_socket(
        core_instance::ConfigIdentity::new(identity).unwrap(),
        core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
        client_config(&[Capability::SnapshotEvents]),
        socket.to_path_buf(),
    )
    .unwrap()
}

const MESSAGE: &str = r#"{"kind":"Message","payload":{"kind":"MessageCompleted","payload":{"run_id":"run-c","text":"t"}}}"#;

/// `decodable` legacy rows (written before the `subkind` and `run_id` columns
/// existed) the backfill converts and `undecodable` ones it cannot decode, in
/// this order, in one transaction.
fn seed(database: &Path, decodable: u32, undecodable: u32) {
    let mut conn = rusqlite::Connection::open(database).unwrap();
    let tx = conn.transaction().unwrap();
    for n in 0..decodable + undecodable {
        let payload = if n < decodable {
            MESSAGE
        } else {
            "undecodable"
        };
        tx.execute(
            "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
             VALUES (1, ?1, ?1, 'Message', ?2)",
            rusqlite::params![i64::from(n) + 1, payload],
        )
        .unwrap();
    }
    tx.commit().unwrap();
}

/// A core serving its socket: the production maintenance area over the events
/// database of `dir`, which holds legacy rows (written before the `subkind` and
/// `run_id` columns existed).
struct Core {
    _dir: test_socket_dir::SplitDir,
    database: PathBuf,
    identity: PathBuf,
    socket: PathBuf,
    connections: Arc<ConnectionHooks>,
    /// Every client whose last connection ended, after the area heard of it.
    departures: tokio::sync::mpsc::UnboundedReceiver<ClientId>,
    /// The storage of a hand-built core, to put threads in its index.
    store: Option<storage::StorageHandle>,
    /// What keeps the core's storage and areas alive.
    _keep: Box<dyn std::any::Any>,
    _instance: evorch_core::instance::Instance,
    _stop: tokio::sync::watch::Sender<bool>,
    server: tokio::task::JoinHandle<()>,
}

impl Core {
    /// `decodable` rows the backfill converts and `undecodable` ones it cannot
    /// decode, in this order.
    async fn start(decodable: u32, undecodable: u32, hook: Option<MaintenanceHook>) -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let database = root.join("events.db");
        let store = storage::Storage::open(storage::StorageConfig {
            db_path: database.clone(),
            ..Default::default()
        })
        .unwrap();
        seed(&database, decodable, undecodable);

        let bus = Arc::new(event_bus::EventBus::new(64));
        let runtime = runtime::AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            Arc::new(Idle),
        );
        let host = workbench_host::CommandHost::new(workbench_host::commands::CommandService::new(
            runtime,
            tokio::runtime::Handle::current(),
        ));
        let connections: Arc<ConnectionHooks> = Arc::default();
        let maintenance = MaintenanceArea::from_parts(
            AreaTasks::new(tokio::runtime::Handle::current()),
            host.projection().source("maintenance").unwrap(),
            store.handle(),
            MaintenanceSeams { hook },
        );
        maintenance.stop_on_disconnect(&connections);
        // Registered after the area's hook: a departure seen here has already
        // been told to the area.
        let (left, departures) = tokio::sync::mpsc::unbounded_channel();
        connections.register(Arc::new(move |client, _| {
            left.send(client.clone()).unwrap();
        }));
        let host = Arc::new(Mutex::new(host.with_areas(Areas {
            connections: connections.clone(),
            maintenance,
            ..Default::default()
        })));
        let handle = store.handle();
        let mut core = Self::serve(
            dir,
            database,
            host,
            connections,
            departures,
            Box::new(store),
        );
        core.store = Some(handle);
        core
    }

    /// A core of the production composition: the host that
    /// `BootstrappedHost::open` builds (its maintenance area and its
    /// connection hooks are the ones the core process has) over the events
    /// database of `dir`, with the same legacy rows. Nothing holds an operation
    /// there, so a test that stops one watches the progress it prints.
    async fn bootstrapped(decodable: u32, undecodable: u32) -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let boot = BootstrappedHost::open(BootstrapOptions {
            config_dir: root.clone(),
            project_root: root.clone(),
            ownership_root: dir.sockets().to_path_buf(),
        })
        .await
        .unwrap();
        let database = root.join("evorch-events.db");
        seed(&database, decodable, undecodable);
        let connections = boot.host.lock().unwrap().connections();
        let (left, departures) = tokio::sync::mpsc::unbounded_channel();
        connections.register(Arc::new(move |client, _| {
            left.send(client.clone()).unwrap();
        }));
        let host = boot.host.clone();
        Self::serve(dir, database, host, connections, departures, Box::new(boot))
    }

    fn serve(
        dir: test_socket_dir::SplitDir,
        database: PathBuf,
        host: Arc<Mutex<workbench_host::CommandHost>>,
        connections: Arc<ConnectionHooks>,
        departures: tokio::sync::mpsc::UnboundedReceiver<ClientId>,
        keep: Box<dyn std::any::Any>,
    ) -> Self {
        let root = dir.path().canonicalize().unwrap();
        let identity = root.join("identity");
        let config = core_instance::ConfigIdentity::new(&identity).unwrap();
        let instance = evorch_core::instance::Instance::acquire(
            &core_instance::LaunchGuard::acquire(&config).unwrap(),
            dir.sockets(),
        )
        .unwrap();
        let listener = instance.bind().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::UnixListener::from_std(listener).unwrap();
        let backend = Arc::new(evorch_core::backend::HostBackend::new(
            host,
            instance.identity.clone(),
        ));
        let (stop, stopped) = tokio::sync::watch::channel(false);
        let server = tokio::spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            while let Ok((stream, _)) = listener.accept().await {
                connections.spawn(evorch_core::unix::connection(
                    stream,
                    backend.clone(),
                    stopped.clone(),
                ));
            }
        });
        Self {
            database,
            identity,
            socket: instance.socket.clone(),
            connections,
            departures,
            store: None,
            _dir: dir,
            _keep: keep,
            _instance: instance,
            _stop: stop,
            server,
        }
    }

    fn launcher(&self) -> core_launcher::Launcher {
        launcher(&self.identity, &self.socket)
    }

    async fn client(&self) -> WorkbenchClient {
        connect_core(&self.launcher()).await.unwrap().client
    }

    /// Legacy rows the backfill has not converted yet, read from the file.
    fn unconverted(&self) -> i64 {
        rusqlite::Connection::open(&self.database)
            .unwrap()
            .query_row(
                "SELECT COUNT(*) FROM events WHERE subkind IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap()
    }

    /// The CLI as a process of its own, connected to this core.
    fn cli(&self, batch_rows: u32, max_rows: Option<u64>) -> std::process::Child {
        let mut argv = format!("maintenance backfill-events --batch-rows {batch_rows}");
        if let Some(max_rows) = max_rows {
            argv = format!("{argv} --max-rows {max_rows}");
        }
        let mut cli = Command::new(std::env::current_exe().unwrap());
        cli.args([
            "--exact",
            "a_cli_process_that_runs_backfill_events",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, &self.socket)
        .env(CHILD_IDENTITY, &self.identity)
        .env(CHILD_ARGV, argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        cli.spawn().unwrap()
    }
}

impl Core {
    /// A verified source run the thread index can take, in the project `/project`.
    fn source(&self, id: &str, at: i64) {
        use storage::thread_index::{
            BlockOrigin, BlockProvenance, MessageProvenance, PROVENANCE_FORMAT_VERSION,
            ProvenanceEvidence, ProvenanceMap, ProvenanceStatus, SnapshotWrite,
        };
        let handle = self.store.as_ref().expect("a hand-built core");
        handle
            .save_snapshot_bundle(&SnapshotWrite {
                record: storage::RunContextRecord {
                    run_id: id.into(),
                    role: "worker".into(),
                    name: "worker".into(),
                    parent_run_id: None,
                    config_json: json!({"thread_id": id}).to_string(),
                    messages_json: json!([
                        {"role": "user", "content": [{"type": "text", "text": "hello"}]}
                    ])
                    .to_string(),
                    checkpoints_json: "[]".into(),
                    terminal_phase: "Done".into(),
                    restorable: true,
                    updated_at_ns: at,
                    host_inputs_json: "[]".into(),
                },
                expected_snapshot_revision: None,
                binding: None,
                status: ProvenanceStatus::Verified {
                    evidence: ProvenanceEvidence::CompletePositionMap,
                },
                map: ProvenanceMap {
                    version: PROVENANCE_FORMAT_VERSION,
                    raw_message_count: 1,
                    checkpoint_count: 0,
                    messages: vec![MessageProvenance {
                        raw_index: 0,
                        ordinal: 0,
                        blocks: vec![BlockProvenance {
                            block_index: 0,
                            origin: BlockOrigin::Native,
                            excluded_reason: None,
                        }],
                    }],
                },
            })
            .unwrap();
        handle
            .upsert_threads_host(&[storage::threads::ThreadUpsert {
                thread_id: id.into(),
                project_root: Some("/project".into()),
                root_run_id: Some(id.into()),
                created_at_ns: at,
                updated_at_ns: at,
                ..Default::default()
            }])
            .unwrap();
    }

    /// `evorch reindex-threads` as a process of its own, connected to this core.
    fn cli_reindex(&self, argv: &str) -> std::process::Child {
        let mut cli = Command::new(std::env::current_exe().unwrap());
        cli.args([
            "--exact",
            "a_cli_process_that_runs_reindex_threads",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, &self.socket)
        .env(CHILD_IDENTITY, &self.identity)
        .env(CHILD_ARGV, format!("reindex-threads {argv}"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        cli.spawn().unwrap()
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        self.server.abort();
    }
}

const CHILD_SOCKET: &str = "MAINTENANCE_CLI_E2E_SOCKET";
const CHILD_IDENTITY: &str = "MAINTENANCE_CLI_E2E_IDENTITY";
const CHILD_ARGV: &str = "MAINTENANCE_CLI_E2E_ARGV";

/// Not a test of its own: the process the tests below start, which stands for
/// `evorch maintenance backfill-events`. It connects to the core named by the
/// environment the way the CLI does (`connect_core` of a launcher), runs the
/// backfill, and prints and exits through `finish`. Without the environment it
/// does nothing.
#[test]
fn a_cli_process_that_runs_backfill_events() {
    let Some(socket) = std::env::var_os(CHILD_SOCKET) else {
        return;
    };
    let identity = PathBuf::from(std::env::var_os(CHILD_IDENTITY).unwrap());
    // The command line the CLI would get, parsed by the CLI's own parser.
    let argv = std::env::var(CHILD_ARGV).unwrap();
    let request = evorch::backfill::parse_args(argv.split(' ').map(str::to_owned)).unwrap();
    let code = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let launcher = launcher(&identity, Path::new(&socket));
            finish(match connect_core(&launcher).await {
                Ok(connected) => backfill(&connected.client, &request).await,
                Err(error) => Err(error),
            })
        });
    std::process::exit(i32::from(code != ExitCode::SUCCESS));
}

/// Not a test of its own: the process the reindex tests start, which stands for
/// `evorch reindex-threads`: the CLI's own parser, task, run and way of ending
/// over the core named by the environment.
#[test]
fn a_cli_process_that_runs_reindex_threads() {
    let Some(socket) = std::env::var_os(CHILD_SOCKET) else {
        return;
    };
    let identity = PathBuf::from(std::env::var_os(CHILD_IDENTITY).unwrap());
    let argv = std::env::var(CHILD_ARGV).unwrap();
    let code = match evorch::reindex_threads::parse_args(argv.split_whitespace().map(str::to_owned))
    {
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(1)
        }
        Ok(args) => {
            let task = evorch::reindex_threads::task(&args).unwrap();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(async {
                    let launcher = launcher(&identity, Path::new(&socket));
                    evorch::cli::reindex::finish(match connect_core(&launcher).await {
                        Ok(connected) => {
                            online_maintenance::reindex(&connected.client, &task).await
                        }
                        Err(error) => Err(error),
                    })
                })
        }
    };
    std::process::exit(i32::from(code != ExitCode::SUCCESS));
}

/// What the reindex CLI wrote to stdout, without the harness lines.
fn reindex_stdout(out: &str) -> &str {
    out.strip_prefix("\nrunning 1 test\ntest a_cli_process_that_runs_reindex_threads ... ")
        .unwrap_or_else(|| panic!("stdout lacks the harness lines: {out:?}"))
}

/// What the CLI wrote to stdout, without the test harness's own lines that
/// come first (the child is a test binary that ends by `process::exit`, so
/// nothing of the harness follows the CLI's output).
fn cli_stdout(out: &str) -> &str {
    out.strip_prefix("\nrunning 1 test\ntest a_cli_process_that_runs_backfill_events ... ")
        .unwrap_or_else(|| panic!("stdout lacks the harness lines: {out:?}"))
}

/// The text the offline command printed for a summary, so that scripts reading
/// it keep working.
fn summary_text(
    converted: u64,
    reattributed: u64,
    undecodable: u64,
    batches: u64,
    remaining: u64,
) -> String {
    serde_json::to_string_pretty(&json!({
        "converted": converted,
        "reattributed": reattributed,
        "undecodable": undecodable,
        "batches": batches,
        "remaining": remaining,
    }))
    .unwrap()
}

async fn settled(events: &mut workbench_client::Subscription) -> MaintenanceView {
    loop {
        let envelope = events.next().await.expect("the subscription stays open");
        if let UiEvent::Maintenance(MaintenanceEvent::MaintenanceSettled { view }) = envelope.event
        {
            return *view;
        }
    }
}

async fn subscribe(client: &WorkbenchClient) -> workbench_client::Subscription {
    client
        .subscribe(Subscribe {
            scope: SubscriptionScope::All,
            after: None,
        })
        .await
        .unwrap()
}

async fn operations(client: &WorkbenchClient) -> Vec<MaintenanceView> {
    let QueryReply::Maintenance(MaintenanceReply::List(page)) = client
        .query(Query::Maintenance(MaintenanceQuery::ListMaintenance(
            ListMaintenance::default(),
        )))
        .await
        .unwrap()
    else {
        panic!("not a list of operations");
    };
    page.items
}

fn converted(view: &MaintenanceView) -> u64 {
    let MaintenanceProgress::BackfillEvents { converted, .. } = &view.progress else {
        panic!("a backfill reports its own progress: {view:?}");
    };
    *converted
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_shows_each_batch_on_stderr_and_the_summary_on_stdout_as_before() {
    // Given: four rows the backfill converts and one it cannot decode, a batch of
    // two rows, and a core that holds the operation after its first batch.
    let hold = Hold::after(1);
    let core = Core::start(4, 1, Some(hold.hook())).await;

    // When: the CLI backfills in a process of its own.
    let mut cli = core.cli(2, None);
    let mut stderr = BufReader::new(cli.stderr.take().unwrap()).lines();
    let mut stdout = cli.stdout.take().unwrap();
    hold.reached().await;

    // Then: the first batch is on stderr while the operation still runs.
    let (first, mut stderr) = tokio::task::spawn_blocking(move || {
        let line = stderr.next().unwrap().unwrap();
        (line, stderr)
    })
    .await
    .unwrap();
    assert_eq!(
        first,
        "backfill-events: batch 1: converted 2, reattributed 0, undecodable 0, remaining 3"
    );
    assert_eq!(core.unconverted(), 3);

    // When: the operation goes on to the end.
    hold.release();
    let (status, lines, out) = tokio::task::spawn_blocking(move || {
        let mut out = String::new();
        stdout.read_to_string(&mut out).unwrap();
        let lines: Vec<String> = stderr.by_ref().map(Result::unwrap).collect();
        (cli.wait().unwrap(), lines, out)
    })
    .await
    .unwrap();

    // Then: the CLI succeeded, stderr ends with the totals, and stdout holds the
    // summary the offline command printed.
    assert!(status.success(), "{status:?}: {lines:?}");
    assert_eq!(
        lines.last().unwrap(),
        "backfill-events: batch 3: converted 5, reattributed 0, undecodable 1, remaining 0"
    );
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("backfill-events: batch ")),
        "{lines:?}"
    );
    let summary = summary_text(5, 0, 1, 3, 0);
    assert_eq!(cli_stdout(&out), format!("{summary}\n"));
    assert_eq!(
        serde_json::from_str::<Value>(&summary).unwrap(),
        json!({"converted":5,"reattributed":0,"undecodable":1,"batches":3,"remaining":0})
    );
    assert_eq!(core.unconverted(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_row_budget_bounds_the_rows_the_cli_converts_and_leaves_the_rest() {
    // Given: ten rows and a budget of four.
    let core = Core::start(10, 0, None).await;

    // When: the CLI runs to its end.
    let cli = core.cli(2, Some(4));
    let output = tokio::task::spawn_blocking(move || cli.wait_with_output().unwrap())
        .await
        .unwrap();

    // Then: four rows were converted, in two batches, and the rest remains.
    let out = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary = summary_text(4, 0, 0, 2, 6);
    assert_eq!(cli_stdout(&out), format!("{summary}\n"));
    assert_eq!(core.unconverted(), 6);
}

/// A CLI invocation that ends by `signal` stops the backfill it started at the
/// next batch boundary, and only that one: a client that came and went, and a
/// client that stays, are not touched, and the one that stays can finish the
/// work afterwards.
async fn a_signalled_cli_stops_only_its_own_backfill(signal: Option<&str>) {
    // Given: ten rows, a core that holds an operation after its first batch of
    // three, and a client that stays connected and listens.
    let hold = Hold::after(1);
    let mut core = Core::start(10, 0, Some(hold.hook())).await;
    let staying = core.client().await;
    let mut events = subscribe(&staying).await;

    // When: a CLI starts its backfill and is held after the first batch.
    let mut cli = core.cli(3, None);
    hold.reached().await;
    let started = operations(&staying).await;
    assert_eq!(started.len(), 1);
    assert!(!started[0].operation.state.is_final());

    // And: a client that started nothing comes and goes.
    let visitor = core.client().await;
    drop(visitor);
    let visitor_id = core.departures.recv().await.unwrap();
    assert!(visitor_id.as_str().starts_with("cli-"));
    let still = operations(&staying).await;
    assert!(
        !still[0].operation.state.is_final(),
        "a client that started nothing stopped the backfill: {still:?}"
    );

    // And: the CLI process is signalled.
    match signal {
        None => cli.kill().unwrap(),
        Some(signal) => {
            let status = Command::new("kill")
                .args([&format!("-{signal}"), &cli.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
    let status = tokio::task::spawn_blocking(move || cli.wait().unwrap())
        .await
        .unwrap();
    assert!(!status.success(), "the CLI did not finish by itself");

    // Then: the core heard of the CLI leaving, and the operation stops at the
    // boundary it is held at, with what it had committed.
    let left = core.departures.recv().await.unwrap();
    assert_ne!(left, visitor_id);
    hold.release();
    let done = settled(&mut events).await;
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    assert_eq!((done.batches, converted(&done)), (1, 3));
    assert_eq!(core.unconverted(), 7);

    // And: the client that stayed was not disconnected and finishes the work.
    assert_eq!(core.connections.connections_of(&visitor_id), 0);
    let rendered = backfill(
        &staying,
        &BackfillRequest {
            user_config_dir: None,
            batch_rows: Some(3),
            max_rows: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(rendered, Ok(summary_text(7, 0, 0, 3, 0)));
    assert_eq!(core.unconverted(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_killed_by_sigkill_stops_only_its_own_backfill() {
    a_signalled_cli_stops_only_its_own_backfill(None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_interrupted_by_sigint_stops_only_its_own_backfill() {
    a_signalled_cli_stops_only_its_own_backfill(Some("INT")).await;
}

// ---- the production composition (`BootstrappedHost`) -----------------------
//
// The tests above hold the operation with a seam of a hand-built area. These
// run the CLI against the host the core process builds, where no seam exists:
// the area, its disconnect hook and the connection counting are the production
// ones.

#[tokio::test(flavor = "multi_thread")]
async fn a_bootstrapped_core_runs_the_cli_backfill_and_prints_the_same_summary() {
    // Given: the production host over four rows to convert and one it cannot decode.
    let core = Core::bootstrapped(4, 1).await;

    // When: the CLI backfills in a process of its own.
    let cli = core.cli(2, None);
    let output = tokio::task::spawn_blocking(move || cli.wait_with_output().unwrap())
        .await
        .unwrap();

    // Then: it succeeded, shows its progress on stderr, and prints the summary on stdout.
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{err}");
    assert!(
        err.lines()
            .last()
            .is_some_and(|line| line.starts_with("backfill-events: batch 3: converted 5,")),
        "{err}"
    );
    let summary = summary_text(5, 0, 1, 3, 0);
    assert_eq!(cli_stdout(&out), format!("{summary}\n"));
    assert_eq!(core.unconverted(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_killed_while_a_bootstrapped_core_backfills_stops_only_its_own_operation() {
    // Given: more batches of one row than the CLI can see through before it is
    // killed, and a client that stays connected and listens.
    const ROWS: u32 = 100_000;
    let mut core = Core::bootstrapped(ROWS, 0).await;
    let staying = core.client().await;
    let mut events = subscribe(&staying).await;

    // When: a CLI starts its backfill and the core has committed a batch (the
    // CLI prints a line once it has heard of one; which batch that is, is not
    // fixed). The reader stays open to the end of the process, so that the CLI
    // never fails writing to a closed pipe: it is the kill that ends it.
    let mut cli = core.cli(1, None);
    let stderr = BufReader::new(cli.stderr.take().unwrap());
    let (first_line, first) = tokio::sync::oneshot::channel();
    let reader = std::thread::spawn(move || {
        let mut first_line = Some(first_line);
        let mut lines = Vec::new();
        for line in stderr.lines() {
            let line = line.unwrap();
            if let Some(first_line) = first_line.take() {
                let _ = first_line.send(line.clone());
            }
            lines.push(line);
        }
        lines
    });
    let first = first.await.unwrap();
    assert!(first.starts_with("backfill-events: batch "), "{first}");

    // And: a client that started nothing comes and goes, then the CLI is killed.
    drop(core.client().await);
    let visitor = core.departures.recv().await.unwrap();
    cli.kill().unwrap();
    let (status, lines) = tokio::task::spawn_blocking(move || {
        let status = cli.wait().unwrap();
        (status, reader.join().unwrap())
    })
    .await
    .unwrap();
    assert_eq!(
        status.signal(),
        Some(9),
        "the CLI was killed, it did not end by itself: {status:?}, {lines:?}"
    );
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("backfill-events: batch ")),
        "everything the CLI said is a progress line: {lines:?}"
    );

    // Then: the operation the CLI started stops at a batch boundary, with what
    // it had committed, and nothing else was stopped.
    let left = core.departures.recv().await.unwrap();
    assert_ne!(left, visitor);
    let done = settled(&mut events).await;
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    let converted = converted(&done);
    assert!((1..u64::from(ROWS)).contains(&converted), "{converted}");
    assert_eq!(
        core.unconverted(),
        i64::from(ROWS) - i64::try_from(converted).unwrap()
    );

    // And: the client that stayed was not disconnected and finishes the work.
    let rendered = backfill(
        &staying,
        &BackfillRequest {
            user_config_dir: None,
            batch_rows: Some(100_000),
            max_rows: None,
        },
    )
    .await
    .unwrap();
    assert!(rendered.is_ok(), "{rendered:?}");
    assert_eq!(core.unconverted(), 0);
}

// ---- reindex-threads (stage 17d-3) -----------------------------------------
//
// `evorch reindex-threads` is a task of the same core: the CLI shows its steps
// on stderr, prints the result it always printed on stdout, and stops what it
// started when it goes away.

/// What the reindex CLI printed to stdout, read as the result the offline
/// command printed: the same JSON the storage's own result serializes to.
fn printed_result(out: &str) -> storage::thread_index::ReindexResult {
    let text = reindex_stdout(out).trim_end();
    let result: storage::thread_index::ReindexResult = serde_json::from_str(text).unwrap();
    assert_eq!(
        text,
        serde_json::to_string_pretty(&result).unwrap(),
        "the printed JSON is the one of the offline result"
    );
    result
}

#[tokio::test(flavor = "multi_thread")]
async fn the_cli_shows_each_reindex_step_on_stderr_and_the_result_on_stdout_as_before() {
    // Given: two threads to index, and a core that holds the operation before
    // its first step.
    let hold = Hold::after(0);
    let core = Core::start(0, 0, Some(hold.hook())).await;
    core.source("first", 10);
    core.source("second", 20);

    // When: the CLI reindexes in a process of its own, and the core goes on.
    let cli = core.cli_reindex("");
    hold.reached().await;
    hold.release();
    let output = tokio::task::spawn_blocking(move || cli.wait_with_output().unwrap())
        .await
        .unwrap();

    // Then: it succeeded, its steps are on stderr.
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{err}");
    let lines: Vec<&str> = err.lines().collect();
    assert!(
        lines
            .iter()
            .all(|line| line.starts_with("reindex-threads: step ")),
        "{lines:?}"
    );
    // The lines are throttled, so the last one need not be the final state:
    // each names the targets frozen at registration, and the steps count up.
    assert!(!lines.is_empty(), "{lines:?}");
    assert!(
        lines
            .iter()
            .all(|line| line.contains(": targets 2, succeeded ")),
        "{lines:?}"
    );
    let steps: Vec<u64> = lines
        .iter()
        .map(|line| {
            let rest = line.strip_prefix("reindex-threads: step ").unwrap();
            rest.split(':').next().unwrap().parse().unwrap()
        })
        .collect();
    assert!(steps.windows(2).all(|pair| pair[0] < pair[1]), "{steps:?}");

    // And: stdout holds the `completed` result of the offline command's shape.
    let storage::thread_index::ReindexResult::Completed { summary, .. } = printed_result(&out)
    else {
        panic!("not a completed result: {out}");
    };
    assert_eq!((summary.target_count, summary.succeeded), (2, 2));
    assert!(summary.gaps.is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bootstrapped_core_runs_the_cli_reindex_and_prints_the_result() {
    // Given: the production host, whose index has no thread yet.
    let core = Core::bootstrapped(0, 0).await;

    // When: the CLI reindexes in a process of its own.
    let cli = core.cli_reindex("--include-evicted");
    let output = tokio::task::spawn_blocking(move || cli.wait_with_output().unwrap())
        .await
        .unwrap();

    // Then: it succeeded with an empty `completed` result.
    let (out, err) = (
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    assert!(output.status.success(), "{err}");
    let storage::thread_index::ReindexResult::Completed { summary, .. } = printed_result(&out)
    else {
        panic!("not a completed result: {out}");
    };
    assert_eq!((summary.target_count, summary.succeeded), (0, 0));
}

/// A CLI invocation that ends by `signal` stops the reindex it started before
/// its next step, and only that one: a client that came and went, and a client
/// that stays, are not touched, and the one that stays reindexes afterwards.
async fn a_signalled_cli_stops_only_its_own_reindex(signal: Option<&str>) {
    // Given: two threads to index, a core that holds an operation before its
    // first step, and a client that stays connected and listens.
    let hold = Hold::after(0);
    let mut core = Core::start(0, 0, Some(hold.hook())).await;
    core.source("first", 10);
    core.source("second", 20);
    let staying = core.client().await;
    let mut events = subscribe(&staying).await;

    // When: a CLI starts its reindex and is held before the first step.
    let mut cli = core.cli_reindex("");
    hold.reached().await;
    let started = operations(&staying).await;
    assert_eq!(started.len(), 1);
    assert!(!started[0].operation.state.is_final());

    // And: a client that started nothing comes and goes.
    drop(core.client().await);
    let visitor = core.departures.recv().await.unwrap();
    assert!(
        !operations(&staying).await[0].operation.state.is_final(),
        "a client that started nothing stopped the reindex"
    );

    // And: the CLI process is signalled.
    match signal {
        None => cli.kill().unwrap(),
        Some(signal) => {
            let status = Command::new("kill")
                .args([&format!("-{signal}"), &cli.id().to_string()])
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
    let status = tokio::task::spawn_blocking(move || cli.wait().unwrap())
        .await
        .unwrap();
    assert!(!status.success(), "the CLI did not finish by itself");

    // Then: the core heard of the CLI leaving, and the reindex stops at the
    // boundary it is held at, without a result.
    let left = core.departures.recv().await.unwrap();
    assert_ne!(left, visitor);
    hold.release();
    let done = settled(&mut events).await;
    assert_eq!(
        done.operation.state,
        OperationState::Cancelled {
            reason: CancelReason::Requested
        }
    );
    let MaintenanceProgress::ReindexThreads { result, .. } = &done.progress else {
        panic!("a reindex reports its own progress: {done:?}");
    };
    assert_eq!(result, &None);

    // And: the client that stayed was not disconnected, and its own reindex
    // takes the threads the stopped one left.
    let end = online_maintenance::reindex(&staying, &ReindexThreads::default())
        .await
        .unwrap();
    let ReindexEnd::Result(ReindexResult::Completed { summary, .. }) = end else {
        panic!("the staying client's reindex did not complete: {end:?}");
    };
    assert_eq!((summary.target_count, summary.succeeded), (2, 2));
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_killed_by_sigkill_stops_only_its_own_reindex() {
    a_signalled_cli_stops_only_its_own_reindex(None).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_cli_interrupted_by_sigint_stops_only_its_own_reindex() {
    a_signalled_cli_stops_only_its_own_reindex(Some("INT")).await;
}
