//! `evorch maintenance backfill-events` against a real core (ADR 0045, stage
//! 17d-2): the CLI's own code (`evorch::online_maintenance`, the entry it shares
//! with `evorch eval`) over the Unix socket of a production maintenance area
//! over a real database.
//!
//! The CLI is a process of its own here (this test binary, started again with
//! `--exact` on the "child" test below), so what it prints and how it ends are
//! observed from outside, and an interruption is a real signal. The area is held
//! between two batches by a gate the test opens, so every wait is for an event
//! the core, the gate or the child sends, never for time to pass.
use std::io::{BufRead as _, BufReader, Read as _};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use evorch::cli::core_client::{client_config, connect_core, finish};
use evorch::online_maintenance::{BackfillRequest, backfill};
use serde_json::{Value, json};
use workbench_client::WorkbenchClient;
use workbench_host::areas::maintenance::{
    MaintenanceArea, MaintenanceHook, MaintenanceSeams, MaintenanceStage,
};
use workbench_host::areas::{AreaTasks, Areas};
use workbench_host::connections::ConnectionHooks;
use workbench_protocol::events::{Subscribe, SubscriptionScope, UiEvent};
use workbench_protocol::handshake::Capability;
use workbench_protocol::ids::ClientId;
use workbench_protocol::namespaces::maintenance::{
    ListMaintenance, MaintenanceEvent, MaintenanceProgress, MaintenanceQuery, MaintenanceReply,
    MaintenanceView,
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
    _store: storage::Storage,
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
        let conn = rusqlite::Connection::open(&database).unwrap();
        for n in 0..decodable + undecodable {
            let payload = if n < decodable {
                MESSAGE
            } else {
                "undecodable"
            };
            conn.execute(
                "INSERT INTO events(schema_version, monotonic_ns, wall_clock_ns, kind, payload) \
                 VALUES (1, ?1, ?1, 'Message', ?2)",
                rusqlite::params![i64::from(n) + 1, payload],
            )
            .unwrap();
        }
        drop(conn);

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
        connections.register(Arc::new(move |client| {
            left.send(client.clone()).unwrap();
        }));
        let host = Arc::new(Mutex::new(host.with_areas(Areas {
            connections: connections.clone(),
            maintenance,
            ..Default::default()
        })));

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
            _dir: dir,
            _store: store,
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
        let mut cli = Command::new(std::env::current_exe().unwrap());
        cli.args([
            "--exact",
            "a_cli_process_that_runs_backfill_events",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD_SOCKET, &self.socket)
        .env(CHILD_IDENTITY, &self.identity)
        .env(CHILD_BATCH_ROWS, batch_rows.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
        if let Some(max_rows) = max_rows {
            cli.env(CHILD_MAX_ROWS, max_rows.to_string());
        }
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
const CHILD_BATCH_ROWS: &str = "MAINTENANCE_CLI_E2E_BATCH_ROWS";
const CHILD_MAX_ROWS: &str = "MAINTENANCE_CLI_E2E_MAX_ROWS";

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
    let number = |name: &str| std::env::var(name).ok().map(|text| text.parse().unwrap());
    let request = BackfillRequest {
        user_config_dir: None,
        batch_rows: number(CHILD_BATCH_ROWS).map(|rows| u32::try_from(rows).unwrap()),
        max_rows: number(CHILD_MAX_ROWS),
    };
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
    let MaintenanceProgress::BackfillEvents { converted, .. } = &view.progress;
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
    assert!(
        out.contains(&format!("{summary}\n")),
        "stdout is {out:?}, which lacks {summary}"
    );
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
    assert!(out.contains(&format!("{summary}\n")), "{out:?}");
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
