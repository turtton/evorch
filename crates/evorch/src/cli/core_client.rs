//! The one way a CLI command reaches the core (ADR 0045 §10): connect to the
//! core that serves this configuration, or start it, and never fall back to
//! working on the configuration database in this process.
//!
//! The module is shared by every command that runs in the core (`eval`, and
//! `reindex-threads` and `maintenance backfill-events` after it). It knows
//! nothing about any namespace:
//!
//! * [`connect`] / [`connect_core`] — connect-or-start through
//!   [`core_launcher::Launcher`], and say whether this client started the core,
//!   met one already running, or lost the launch race to another client
//!   ([`Attach`]). Racing launchers, a core that exits with "already running"
//!   and the single cold-start attempt are the launcher's; this layer never
//!   spawns a second core and never retries a refusal.
//! * [`client_config`] — who the CLI says it is: a per-process client id and the
//!   `evorch-cli` build. The core attaches provenance from the connection; the
//!   CLI sends none (see `workbench_protocol::envelope::Provenance`).
//! * [`send`] / [`ask`] — one command or query, with the failure classes of
//!   [`CliError`]. A command whose reply was lost is **never** sent again: the
//!   outcome is unknown ([`CliError::OutcomeUnknown`]) and the caller says so.
//! * [`wait_settled`] — wait for an operation to settle without a clock: the
//!   caller reads the operation, and between reads this waits for an event it
//!   names. A lost connection or an operation the core no longer knows is an
//!   unknown outcome, not a failure and not a success.
//! * [`exit`] — the exit codes of ADR 0045 §10.
//!
//! Nothing here waits for time; every wait is for a message the core sends.
use std::fmt;
use std::future::Future;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use core_launcher::{CoreBinary, LaunchError, LaunchStage, Launcher};
use workbench_client::{
    ClientConfig, ClientError, ConnectionState, Subscription, TransportError, WorkbenchClient,
};
use workbench_protocol::{
    ErrorCode, RetryAdvice, WorkbenchError,
    commands::{Command, CommandReply},
    events::{Subscribe, SubscriptionScope, UiEvent},
    handshake::{BuildInfo, Capability},
    ids::{ClientId, OperationId, RequestId},
    queries::{Query, QueryReply},
};

/// The build name the CLI introduces itself with.
pub const CLI_BUILD_NAME: &str = "evorch-cli";

/// Exit codes of a CLI command that ran in the core (ADR 0045 §10).
pub mod exit {
    use std::process::ExitCode;

    pub const SUCCESS: u8 = 0;
    pub const FAILURE: u8 = 1;
    /// `--no-wait`, and the core asked for input.
    pub const WAITING_INPUT: u8 = 3;
    /// Ctrl-C.
    pub const INTERRUPTED: u8 = 130;

    pub fn code(code: u8) -> ExitCode {
        ExitCode::from(code)
    }
}

/// Why a CLI command could not finish. Every kind is a failure of the command
/// (exit 1); the kinds differ in what the caller may tell the person to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// No core could be reached or started. There is no embedded fallback.
    Unavailable(String),
    /// The core understood the request and refused it.
    Refused(String),
    /// The connection ended before the answer: the request may or may not have
    /// run, and it was not sent again. Ask the core (or read the result) before
    /// doing it a second time.
    OutcomeUnknown {
        /// The request that was in flight, when it is known.
        request: Option<RequestId>,
        detail: String,
    },
    /// The connection was already closed; nothing of this request was sent.
    Disconnected,
    /// Anything else (an invalid request, a protocol violation).
    Failed(String),
}

impl CliError {
    pub fn exit_code(&self) -> ExitCode {
        exit::code(exit::FAILURE)
    }

    fn unknown(detail: impl Into<String>) -> Self {
        Self::OutcomeUnknown {
            request: None,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(detail) => write!(
                f,
                "cannot reach the evorch core ({detail}); evorch never works without it"
            ),
            Self::Refused(detail) => write!(f, "the core refused the request: {detail}"),
            Self::OutcomeUnknown { request, detail } => {
                write!(f, "outcome unknown: {detail}")?;
                if let Some(request) = request {
                    write!(f, " (request {request})")?;
                }
                write!(f, "; it was not sent again")
            }
            Self::Disconnected => write!(f, "disconnected from the core; nothing was sent"),
            Self::Failed(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for CliError {}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Transport(TransportError::OutcomeUnknown { request_id }) => {
                Self::OutcomeUnknown {
                    request: Some(request_id),
                    detail: "the connection ended before the core answered".into(),
                }
            }
            ClientError::Transport(TransportError::Disconnected) => Self::Disconnected,
            ClientError::Server(error) => Self::Refused(error.to_string()),
            other => Self::Failed(other.to_string()),
        }
    }
}

/// How this client came to be connected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attach {
    /// A core was already serving the configuration.
    Existing,
    /// This client started the core.
    Started,
    /// Another client was starting the core (or won the race against this
    /// client's own attempt); this client attached to that one.
    Joined,
}

pub struct Connected {
    pub client: WorkbenchClient,
    pub attach: Attach,
}

/// What introduces a client to a core: connect-or-start of one configuration.
#[async_trait]
pub trait CoreConnector: Send + Sync {
    async fn connect(
        &self,
        observe: &(dyn Fn(LaunchStage) + Send + Sync),
    ) -> Result<WorkbenchClient, LaunchError>;
}

#[async_trait]
impl CoreConnector for Launcher {
    async fn connect(
        &self,
        observe: &(dyn Fn(LaunchStage) + Send + Sync),
    ) -> Result<WorkbenchClient, LaunchError> {
        self.connect_or_start_observed(observe).await
    }
}

/// Connect to the core `connector` names, starting it when none runs.
///
/// # Errors
/// [`CliError::Unavailable`] when the launcher gives up; the CLI never starts
/// the work itself instead.
pub async fn connect_core<C: CoreConnector + ?Sized>(connector: &C) -> Result<Connected, CliError> {
    let spawned = AtomicBool::new(false);
    let met_other = AtomicBool::new(false);
    let client = connector
        .connect(&|stage| match stage {
            LaunchStage::Spawned(_) => spawned.store(true, Ordering::Relaxed),
            // Another client holds the launch or the lifetime, or the core this
            // client started lost the race: attach to the one that won.
            LaunchStage::WaitingForLaunch
            | LaunchStage::LifetimeBusy
            | LaunchStage::AlreadyRunning => met_other.store(true, Ordering::Relaxed),
            LaunchStage::BeforeSpawn => {}
        })
        .await
        .map_err(|error| CliError::Unavailable(error.to_string()))?;
    let attach = if met_other.load(Ordering::Relaxed) {
        Attach::Joined
    } else if spawned.load(Ordering::Relaxed) {
        Attach::Started
    } else {
        Attach::Existing
    };
    Ok(Connected { client, attach })
}

/// What the CLI tells the core about itself. `extra` are capabilities the
/// command needs beyond commands and queries.
pub fn client_config(extra: &[Capability]) -> ClientConfig {
    let mut config = ClientConfig::new(
        ClientId::new(format!("cli-{}", uuid::Uuid::new_v4())).expect("a uuid fits a client id"),
        BuildInfo {
            name: CLI_BUILD_NAME.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: None,
        },
    );
    for capability in extra {
        if !config.required_capabilities.contains(capability) {
            config.required_capabilities.push(*capability);
        }
    }
    config
}

/// The launcher for the core of `config_dir` (the user configuration directory
/// when `None`), started from the binary next to this one.
///
/// # Errors
/// The directory cannot be resolved, or the core binary is missing or unsafe.
pub fn launcher(config_dir: Option<PathBuf>, extra: &[Capability]) -> Result<Launcher, CliError> {
    launcher_with(config_dir, client_config(extra))
}

/// As [`launcher`] with a client configuration of the caller's own (`evorch
/// run` declares itself the command line, which the core records as the
/// surface of what it starts).
pub fn launcher_with(
    config_dir: Option<PathBuf>,
    client: ClientConfig,
) -> Result<Launcher, CliError> {
    let dir = config_dir.or_else(config::user_config_dir).ok_or_else(|| {
        CliError::Failed(
            "cannot resolve the user config directory; pass --user-config <dir>".into(),
        )
    })?;
    let identity = core_instance::ConfigIdentity::new(&dir)
        .map_err(|error| CliError::Unavailable(error.to_string()))?;
    let binary = CoreBinary::sibling().map_err(|error| CliError::Unavailable(error.to_string()))?;
    Launcher::new(identity, binary, client)
        .map_err(|error| CliError::Unavailable(error.to_string()))
}

/// Connect-or-start for `config_dir`; see [`launcher`] and [`connect_core`].
///
/// # Errors
/// As [`launcher`] and [`connect_core`].
pub async fn connect(
    config_dir: Option<PathBuf>,
    extra: &[Capability],
) -> Result<Connected, CliError> {
    connect_core(&launcher(config_dir, extra)?).await
}

/// Send one command. It is never sent again: a reply lost to a disconnect is
/// [`CliError::OutcomeUnknown`].
///
/// # Errors
/// See [`CliError`].
pub async fn send(client: &WorkbenchClient, command: Command) -> Result<CommandReply, CliError> {
    client.command(command).await.map_err(CliError::from)
}

/// Ask one query.
///
/// # Errors
/// See [`CliError`].
pub async fn ask(client: &WorkbenchClient, query: Query) -> Result<QueryReply, CliError> {
    client.query(query).await.map_err(CliError::from)
}

/// The text a finished command leaves: `Ok` goes to stdout (exit 0), `Err` to
/// stderr (exit 1).
pub type Rendered = Result<String, String>;

/// The exit code of a command that ran in the core, after printing its result:
/// `Ok(Ok(text))` to stdout, a job that ran and failed (`Ok(Err(text))`) and a
/// command that could not be carried out to stderr.
pub fn finish(result: Result<Rendered, CliError>) -> ExitCode {
    match result {
        Ok(Ok(text)) => {
            println!("{text}");
            exit::code(exit::SUCCESS)
        }
        Ok(Err(text)) => {
            eprintln!("{text}");
            exit::code(exit::FAILURE)
        }
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    }
}

/// A fresh id for an operation this invocation starts: `prefix` names the
/// command, the rest is random. The core attributes the operation to this
/// connection's client, so a retry of the same invocation never collides with
/// another one's.
///
/// # Errors
/// The prefix does not form a valid id.
pub fn operation_id(prefix: &str) -> Result<OperationId, CliError> {
    OperationId::new(format!("{prefix}-{}", uuid::Uuid::new_v4()))
        .map_err(|error| CliError::Failed(error.to_string()))
}

/// Read one operation: `pick` takes the view out of the reply of `query`.
/// `None` when the core does not hold the operation (it restarted, or the
/// operation left its table), so whether it finished is not known.
///
/// # Errors
/// A refusal other than "not found", a lost connection, or a reply of another
/// kind than `query` asks for.
pub async fn read_operation<V>(
    client: &WorkbenchClient,
    query: Query,
    pick: impl FnOnce(QueryReply) -> Option<V>,
) -> Result<Option<V>, ClientError> {
    match client.query(query).await {
        Ok(reply) => pick(reply).map(Some).ok_or_else(|| {
            ClientError::Server(WorkbenchError::new(
                ErrorCode::Internal,
                "protocol violation: the core did not answer with the operation asked for",
                RetryAdvice::Never,
            ))
        }),
        Err(ClientError::Server(error)) if error.code == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

/// What one read of an operation found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step<T> {
    /// Still running; wait for the next event.
    Pending,
    /// Ended; this is what it left.
    Settled(T),
    /// The core does not know the operation (it restarted, or it left the
    /// table). Whether it finished is not known.
    Unknown,
}

/// A failure of reading or following an operation. Whatever ended the
/// connection (a clean close, a reply lost, a frame cut off in the middle, a
/// frame the client refuses) says nothing about whether the operation ran, so
/// all of them are an unknown outcome. Only the core's own refusal and a
/// request this client built wrongly are failures of the command. A full
/// request queue closes nothing: it is a failure to retry later, not a loss.
fn lost(error: ClientError) -> CliError {
    match error {
        ClientError::Transport(
            TransportError::QueueFull | TransportError::OutcomeUnknown { .. },
        ) => CliError::from(error),
        ClientError::Transport(cause) => CliError::unknown(format!(
            "the connection ended while the operation was running ({cause}); the core may still be running it"
        )),
        other => CliError::from(other),
    }
}

async fn subscribe(
    client: &WorkbenchClient,
    scope: &SubscriptionScope,
) -> Result<Subscription, CliError> {
    client
        .subscribe(Subscribe {
            scope: scope.clone(),
            after: None,
        })
        .await
        .map_err(lost)
}

/// Wait until `read` reports the operation settled.
///
/// The subscription is opened before the first read, so a change between a read
/// and the wait is never missed; after each event `relevant` accepts, the
/// operation is read again. A subscription that ends while the connection lives
/// (a slow subscriber, a new epoch) is replaced and the operation read again.
///
/// # Errors
/// [`CliError::OutcomeUnknown`] when the connection is lost or `read` says
/// [`Step::Unknown`]; the other errors of `read` as they are.
pub async fn wait_settled<T, R, Fut>(
    client: &WorkbenchClient,
    scope: SubscriptionScope,
    relevant: impl Fn(&UiEvent) -> bool,
    read: R,
) -> Result<T, CliError>
where
    R: FnMut() -> Fut,
    Fut: Future<Output = Result<Step<T>, ClientError>>,
{
    wait_settled_every(client, scope, relevant, None, read).await
}

/// As [`wait_settled`], and `read` is also called every `poll` when no event
/// came: for an operation whose changes the event stream does not always
/// announce (an event is a wake-up, never the only way to learn the outcome).
///
/// # Errors
/// As [`wait_settled`].
pub async fn wait_settled_every<T, R, Fut>(
    client: &WorkbenchClient,
    scope: SubscriptionScope,
    relevant: impl Fn(&UiEvent) -> bool,
    poll: Option<Duration>,
    mut read: R,
) -> Result<T, CliError>
where
    R: FnMut() -> Fut,
    Fut: Future<Output = Result<Step<T>, ClientError>>,
{
    let mut subscription = subscribe(client, &scope).await?;
    loop {
        match read().await.map_err(lost)? {
            Step::Settled(value) => return Ok(value),
            Step::Unknown => {
                return Err(CliError::unknown(
                    "the core no longer knows the operation (it may have restarted)",
                ));
            }
            Step::Pending => {}
        }
        loop {
            let next = match poll {
                Some(every) => match tokio::time::timeout(every, subscription.next()).await {
                    Ok(next) => next,
                    Err(_) => break,
                },
                None => subscription.next().await,
            };
            match next {
                Some(envelope) if envelope.event.ends_subscription() => {}
                Some(envelope) if relevant(&envelope.event) => break,
                Some(_) => continue,
                None => {}
            }
            // The subscription is over. Without a connection the outcome is
            // unknown; with one, follow again and read what was missed.
            if let ConnectionState::Closed(_) = client.connection_state() {
                return Err(CliError::unknown(
                    "the connection ended while the operation was running; the core may still be running it",
                ));
            }
            subscription = subscribe(client, &scope).await?;
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use workbench_client::fake::{FakeServer, FakeTransport};
    use workbench_protocol::{
        envelope::{ReplyPayload, ReplyResult, RequestBody},
        events::{EventCursor, EventEnvelope, SubscriptionStarted},
        handshake::{
            ActivityCounts, CoreState, CoreStatus, ProtocolVersion, ServerHello, WireLimits,
        },
        ids::{CoreIdentity, SubscriptionId},
        namespaces::LifecycleEvent,
        wire::{InstanceEpoch, Sequence},
    };

    fn status() -> CoreStatus {
        CoreStatus {
            state: CoreState::Ready,
            retention_reasons: vec![],
            running: ActivityCounts::default(),
        }
    }

    fn hello() -> ServerHello {
        ServerHello {
            protocol_version: ProtocolVersion::CURRENT,
            capabilities: Capability::IMPLEMENTED.to_vec(),
            server_build: BuildInfo {
                name: "core".into(),
                version: "0".into(),
                revision: None,
            },
            core: CoreIdentity::new("core-abc").unwrap(),
            instance_epoch: InstanceEpoch(7),
            limits: WireLimits::default(),
            status: status(),
        }
    }

    async fn connected() -> (WorkbenchClient, FakeServer) {
        let (transport, mut server) = FakeTransport::pair();
        let config = client_config(&[]);
        let hello = hello();
        let (client, _) = tokio::join!(
            async { WorkbenchClient::connect(transport, config).await.unwrap() },
            server.handshake(&hello),
        );
        (client, server)
    }

    fn cursor(sequence: u64) -> EventCursor {
        EventCursor {
            core_epoch: InstanceEpoch(7),
            sequence: Sequence(sequence),
        }
    }

    /// Follow events through the fake core: the subscribe request is answered
    /// with `id`, and the returned closure sends one status event on it.
    async fn serve_subscribe(server: &mut FakeServer, id: &str) {
        let request = server.recv_request().await.expect("subscribe request");
        assert!(matches!(request.body, RequestBody::Control(_)));
        server.reply(
            &request.request_id,
            ReplyResult::Success(ReplyPayload::SubscriptionStarted(SubscriptionStarted {
                subscription_id: SubscriptionId::new(id).unwrap(),
                snapshot_id: None,
                high_water: cursor(0),
                initial_page: None,
            })),
        );
    }

    fn status_event(id: &str, sequence: u64) -> EventEnvelope {
        EventEnvelope {
            subscription_id: SubscriptionId::new(id).unwrap(),
            cursor: cursor(sequence),
            caused_by_request_id: None,
            event: UiEvent::Lifecycle(LifecycleEvent::CoreStatusChanged { status: status() }),
        }
    }

    fn is_status(event: &UiEvent) -> bool {
        matches!(event, UiEvent::Lifecycle(_))
    }

    /// A connector that plays the stages of one launch attempt, then connects
    /// to a fake core.
    struct Scripted {
        stages: Vec<LaunchStage>,
        outcome: Mutex<Option<Result<WorkbenchClient, LaunchError>>>,
    }

    #[async_trait]
    impl CoreConnector for Scripted {
        async fn connect(
            &self,
            observe: &(dyn Fn(LaunchStage) + Send + Sync),
        ) -> Result<WorkbenchClient, LaunchError> {
            for stage in &self.stages {
                observe(*stage);
            }
            self.outcome.lock().unwrap().take().expect("connected once")
        }
    }

    async fn attach_after(stages: &[LaunchStage]) -> Attach {
        let (client, _server) = connected().await;
        let connector = Scripted {
            stages: stages.to_vec(),
            outcome: Mutex::new(Some(Ok(client))),
        };
        connect_core(&connector).await.unwrap().attach
    }

    #[tokio::test]
    async fn the_attach_says_who_started_the_core() {
        assert_eq!(attach_after(&[]).await, Attach::Existing);
        assert_eq!(
            attach_after(&[LaunchStage::BeforeSpawn, LaunchStage::Spawned(10)]).await,
            Attach::Started
        );
        // Another client holds the launch: this one waits and attaches.
        assert_eq!(
            attach_after(&[LaunchStage::WaitingForLaunch]).await,
            Attach::Joined
        );
        assert_eq!(
            attach_after(&[LaunchStage::LifetimeBusy]).await,
            Attach::Joined
        );
    }

    #[tokio::test]
    async fn a_core_that_was_already_running_is_joined_not_failed() {
        // The core this client spawned exited with "already running": the
        // launcher reconnects to the owner. That is a success that joined, and
        // this client did not start the core that serves it.
        let attach = attach_after(&[
            LaunchStage::BeforeSpawn,
            LaunchStage::Spawned(10),
            LaunchStage::AlreadyRunning,
        ])
        .await;
        assert_eq!(attach, Attach::Joined);
    }

    #[tokio::test]
    async fn of_two_racing_launches_one_starts_and_one_joins() {
        // One launch lock: whoever takes it spawns, the other waits and attaches.
        struct Arbiter {
            spawned: AtomicBool,
            cores: Mutex<Vec<WorkbenchClient>>,
        }
        #[async_trait]
        impl CoreConnector for Arbiter {
            async fn connect(
                &self,
                observe: &(dyn Fn(LaunchStage) + Send + Sync),
            ) -> Result<WorkbenchClient, LaunchError> {
                if self.spawned.swap(true, Ordering::SeqCst) {
                    observe(LaunchStage::WaitingForLaunch);
                } else {
                    observe(LaunchStage::BeforeSpawn);
                    observe(LaunchStage::Spawned(1));
                }
                Ok(self.cores.lock().unwrap().pop().expect("a client"))
            }
        }
        let (one, _s1) = connected().await;
        let (two, _s2) = connected().await;
        let arbiter = Arbiter {
            spawned: AtomicBool::new(false),
            cores: Mutex::new(vec![one, two]),
        };
        let (a, b) = tokio::join!(connect_core(&arbiter), connect_core(&arbiter));
        let mut attaches = [a.unwrap().attach, b.unwrap().attach];
        attaches.sort_by_key(|attach| *attach as u8);
        assert_eq!(attaches, [Attach::Started, Attach::Joined]);
    }

    #[tokio::test]
    async fn a_launcher_that_gives_up_is_unavailable_and_no_work_starts() {
        let connector = Scripted {
            stages: vec![LaunchStage::BeforeSpawn],
            outcome: Mutex::new(Some(Err(LaunchError::NotReady))),
        };
        let Err(error) = connect_core(&connector).await else {
            panic!("a failed launch must not connect");
        };
        assert!(matches!(error, CliError::Unavailable(_)), "{error:?}");
        assert!(error.to_string().contains("never works without it"));
    }

    #[test]
    fn the_client_introduces_itself_as_the_cli_and_asks_for_what_the_command_needs() {
        let config = client_config(&[Capability::Commands, Capability::Conversation]);
        assert_eq!(config.client_build.name, CLI_BUILD_NAME);
        assert!(config.client_instance_id.as_str().starts_with("cli-"));
        assert_eq!(
            config.required_capabilities,
            [
                Capability::Commands,
                Capability::Queries,
                Capability::Conversation
            ]
        );
        assert_ne!(
            client_config(&[]).client_instance_id,
            client_config(&[]).client_instance_id
        );
    }

    #[test]
    fn every_failure_is_exit_one() {
        for error in [
            CliError::Unavailable(String::new()),
            CliError::Refused(String::new()),
            CliError::unknown(""),
            CliError::Disconnected,
            CliError::Failed(String::new()),
        ] {
            assert_eq!(error.exit_code(), ExitCode::from(1), "{error:?}");
        }
        assert_eq!(
            [
                exit::SUCCESS,
                exit::FAILURE,
                exit::WAITING_INPUT,
                exit::INTERRUPTED
            ],
            [0, 1, 3, 130]
        );
    }

    #[tokio::test]
    async fn the_operation_is_read_again_after_each_relevant_event_until_it_settles() {
        let (client, mut server) = connected().await;
        let reads = Mutex::new(0_u32);
        let waiting = wait_settled(&client, SubscriptionScope::All, is_status, || {
            let mut reads = reads.lock().unwrap();
            *reads += 1;
            let step = if *reads < 3 {
                Step::Pending
            } else {
                Step::Settled("done")
            };
            async move { Ok(step) }
        });
        let core = async {
            serve_subscribe(&mut server, "sub-1").await;
            // The first read finds the operation pending; one event wakes the
            // second read, another wakes the third.
            server.event(status_event("sub-1", 1));
            server.event(status_event("sub-1", 2));
        };
        let (settled, ()) = tokio::join!(waiting, core);
        assert_eq!(settled.unwrap(), "done");
        assert_eq!(*reads.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn an_event_the_caller_does_not_name_does_not_wake_a_read() {
        let (client, mut server) = connected().await;
        let reads = Mutex::new(0_u32);
        let waiting = wait_settled(
            &client,
            SubscriptionScope::All,
            |_| false,
            || {
                *reads.lock().unwrap() += 1;
                async { Ok(Step::<()>::Pending) }
            },
        );
        let core = async {
            serve_subscribe(&mut server, "sub-1").await;
            server.event(status_event("sub-1", 1));
            // Closing is the only thing that ends the wait; the event before it
            // read nothing.
            server.close();
        };
        let (outcome, ()) = tokio::join!(waiting, core);
        assert!(matches!(
            outcome,
            Err(CliError::OutcomeUnknown { request: None, .. })
        ));
        assert_eq!(*reads.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn a_lost_connection_while_waiting_is_an_unknown_outcome() {
        let (client, mut server) = connected().await;
        let waiting = wait_settled(&client, SubscriptionScope::All, is_status, || async {
            Ok(Step::<()>::Pending)
        });
        let core = async {
            serve_subscribe(&mut server, "sub-1").await;
            server.close();
        };
        let (outcome, ()) = tokio::join!(waiting, core);
        let error = outcome.unwrap_err();
        assert!(
            matches!(error, CliError::OutcomeUnknown { request: None, .. }),
            "{error:?}"
        );
        assert!(error.to_string().contains("was not sent again"));
    }

    /// What a core that dies in the middle of writing a reply leaves on the wire.
    fn cut_off_reply(server: &mut FakeServer) {
        let mut cut = 64u32.to_be_bytes().to_vec();
        cut.extend_from_slice(b"{\"reply\"");
        server.send_raw(cut);
        server.close();
    }

    #[tokio::test]
    async fn a_connection_cut_off_in_the_middle_of_a_frame_is_an_unknown_outcome() {
        // The connection is lost with a read in flight: the client reports a
        // protocol violation for the half frame, but whether the operation ran
        // is exactly as unknown as after a clean close.
        let (client, mut server) = connected().await;
        let waiting = wait_settled(&client, SubscriptionScope::All, is_status, || async {
            client
                .query(Query::Lifecycle(
                    workbench_protocol::namespaces::LifecycleQuery::GetCoreStatus,
                ))
                .await
                .map(|_| Step::<()>::Pending)
        });
        let core = async {
            serve_subscribe(&mut server, "sub-1").await;
            let request = server.recv_request().await.expect("the read");
            assert!(matches!(request.body, RequestBody::Query(_)));
            cut_off_reply(&mut server);
        };
        let (outcome, ()) = tokio::join!(waiting, core);
        let error = outcome.unwrap_err();
        assert!(
            matches!(error, CliError::OutcomeUnknown { request: None, .. }),
            "{error:?}"
        );
        assert_eq!(error.exit_code(), ExitCode::from(1));
    }

    #[tokio::test]
    async fn a_connection_cut_off_while_subscribing_is_an_unknown_outcome() {
        let (client, mut server) = connected().await;
        let waiting = wait_settled(&client, SubscriptionScope::All, is_status, || async {
            Ok(Step::<()>::Pending)
        });
        let core = async {
            let request = server.recv_request().await.expect("subscribe request");
            assert!(matches!(request.body, RequestBody::Control(_)));
            cut_off_reply(&mut server);
        };
        let (outcome, ()) = tokio::join!(waiting, core);
        assert!(matches!(
            outcome,
            Err(CliError::OutcomeUnknown { request: None, .. })
        ));
    }

    #[tokio::test]
    async fn a_full_request_queue_is_a_failure_not_a_lost_connection() {
        let error = lost(ClientError::Transport(TransportError::QueueFull));
        assert!(matches!(error, CliError::Failed(_)), "{error:?}");
    }

    #[tokio::test]
    async fn an_operation_the_core_does_not_know_is_an_unknown_outcome_not_a_success() {
        let (client, mut server) = connected().await;
        let waiting = wait_settled(&client, SubscriptionScope::All, is_status, || async {
            Ok(Step::<()>::Unknown)
        });
        let (outcome, ()) = tokio::join!(waiting, serve_subscribe(&mut server, "sub-1"));
        assert!(matches!(
            outcome,
            Err(CliError::OutcomeUnknown { request: None, .. })
        ));
    }

    #[tokio::test]
    async fn a_command_whose_reply_is_lost_is_unknown_and_never_sent_again() {
        let (client, mut server) = connected().await;
        let command =
            Command::Execution(workbench_protocol::namespaces::ExecutionCommand::StopChat(
                workbench_protocol::commands::ThreadTarget {
                    thread: workbench_protocol::ids::ThreadId::new("thr-1").unwrap(),
                },
            ));
        let sending = send(&client, command);
        let core = async {
            // The core received the command and then went away without a reply.
            let request = server.recv_request().await.expect("the command");
            assert!(matches!(request.body, RequestBody::Command(_)));
            server.close();
            request.request_id
        };
        let (outcome, sent) = tokio::join!(sending, core);
        match outcome.unwrap_err() {
            CliError::OutcomeUnknown { request, .. } => assert_eq!(request, Some(sent)),
            other => panic!("{other:?}"),
        }
        // Exactly one request was ever written: the client has no retry. Once
        // the client is gone nothing more can arrive.
        drop(client);
        assert!(server.recv_request().await.is_none());
    }
}
