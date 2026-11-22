//! `evorch run` against a real core over its Unix socket (ADR 0045 §10, stage 17):
//! the CLI's own code (`evorch::cli::run::execute`) is the client, the host is the
//! production `CommandHost` with a scripted model.
//!
//! What an invocation owns, what ends it, and how a restart is kept apart from
//! it are observed from outside: the GUI-side client (a second connection, with
//! a run of its own) must keep running whatever the CLI does. Every wait is for
//! an event the model, the core or a task sends; the timeouts only bound a
//! failing test.
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use event_bus::AgentRunPhase;
use evorch::cli::core_client::connect_core;
use evorch::cli::run::{Decision, Outcome, Prompter, RunArgs, execute, run_client_config};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as ProviderRole, ToolSpec,
};
use runtime::{AgentInvocationContext, AgentModel, AgentRuntime, Role, RunId, RuntimeError};
use workbench_client::{ClientConfig, UnixTransport, WorkbenchClient};
use workbench_host::CommandHost;
use workbench_host::commands::CommandService;
use workbench_host::projects::StaticProjects;
use workbench_protocol::commands::{
    AnswerUserQuestion, Command, CommandResult, ComposerRole, SendChat,
};
use workbench_protocol::handshake::BuildInfo;
use workbench_protocol::ids::{ClientId, InvocationId, ProjectId, SubmissionId, ThreadId};
use workbench_protocol::namespaces::execution::ExecutionResult;
use workbench_protocol::namespaces::execution::invocation::{
    EndInvocation, EndReason, InvocationRole, StartInvocation,
};
use workbench_protocol::namespaces::{
    ConversationCommand, ExecutionCommand, QuestionsCommand, QuestionsQuery, QuestionsReply,
};
use workbench_protocol::queries::{PendingQuery, Query, QueryReply};
use workbench_protocol::views::{ApprovalView, QuestionView};

const WAIT: Duration = Duration::from_secs(60);

/// Holds a run whose prompt says `hold`, asks a blocking question when it says
/// `ask` (once), and answers everything else.
struct Model {
    reached: tokio::sync::Semaphore,
}

fn prompt_of(messages: &[Message]) -> String {
    messages
        .iter()
        .filter(|m| m.role == ProviderRole::User)
        .flat_map(|m| m.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl AgentModel for Model {
    async fn complete(
        &self,
        _: &AgentInvocationContext,
        _: Role,
        messages: &[Message],
        _: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        let prompt = prompt_of(messages);
        if prompt.contains("hold") {
            self.reached.add_permits(1);
            std::future::pending::<()>().await;
        }
        let answered = messages
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        let (content, finish_reason) = if prompt.contains("ask") && !answered {
            (
                ContentBlock::ToolUse {
                    id: "ask-1".into(),
                    name: "ask_user".into(),
                    input: serde_json::json!({
                        "title": "Which format?",
                        "options": ["JSON", "YAML"],
                        "blocking": true
                    }),
                },
                FinishReason::ToolUse,
            )
        } else {
            (
                ContentBlock::Text {
                    text: "all done".into(),
                },
                FinishReason::Stop,
            )
        };
        Ok(ChatResponse {
            message: Message {
                role: ProviderRole::Assistant,
                content: vec![content],
            },
            finish_reason,
            usage: Default::default(),
            usage_reported: true,
        })
    }

    fn selected_model(&self, _: Role, _: Option<&str>) -> String {
        "run-cli-model".into()
    }
}

/// Answers the questions it is asked, and records them.
#[derive(Default)]
struct Typed {
    answers: Mutex<Vec<String>>,
}

#[async_trait]
impl Prompter for Typed {
    async fn answer(&self, question: &QuestionView) -> Option<String> {
        self.answers.lock().unwrap().push(question.title.clone());
        Some("YAML".into())
    }
    async fn approve(&self, _: &ApprovalView, _: Option<&str>) -> Decision {
        Decision::Deny
    }
}

/// Waits for an answer that never comes from the terminal.
#[derive(Default)]
struct Stuck {
    asked: tokio::sync::Notify,
}

#[async_trait]
impl Prompter for Stuck {
    async fn answer(&self, _: &QuestionView) -> Option<String> {
        self.asked.notify_one();
        std::future::pending().await
    }
    async fn approve(&self, _: &ApprovalView, _: Option<&str>) -> Decision {
        std::future::pending().await
    }
}

/// Says `decision` to every approval, and records what it was shown.
struct Approver {
    decision: Decision,
    /// (summary cut short, length of the complete input shown, if any)
    seen: Mutex<Vec<(bool, Option<usize>)>>,
    shown: tokio::sync::Notify,
}

impl Approver {
    fn new(decision: Decision) -> Self {
        Self {
            decision,
            seen: Mutex::default(),
            shown: tokio::sync::Notify::new(),
        }
    }
}

#[async_trait]
impl Prompter for Approver {
    async fn answer(&self, _: &QuestionView) -> Option<String> {
        None
    }
    async fn approve(&self, approval: &ApprovalView, input: Option<&str>) -> Decision {
        self.seen
            .lock()
            .unwrap()
            .push((approval.summary_truncated, input.map(str::len)));
        self.shown.notify_one();
        self.decision
    }
}

struct Core {
    runtime: AgentRuntime,
    model: Arc<Model>,
    host: Arc<Mutex<CommandHost>>,
    root: PathBuf,
    identity: PathBuf,
    socket: PathBuf,
    _store: storage::Storage,
    _dir: test_socket_dir::SplitDir,
    _instance: evorch_core::instance::Instance,
    _stop: tokio::sync::watch::Sender<bool>,
    _server: tokio::task::JoinHandle<()>,
}

impl Core {
    async fn start() -> Self {
        let dir = test_socket_dir::SplitDir::new();
        let root = dir.path().canonicalize().unwrap();
        let model = Arc::new(Model {
            reached: tokio::sync::Semaphore::new(0),
        });
        let bus = Arc::new(event_bus::EventBus::new(256));
        let storage_config = storage::StorageConfig {
            db_path: root.join("events.db"),
            ..Default::default()
        };
        let store = storage::Storage::open(storage_config.clone()).unwrap();
        let runtime = AgentRuntime::new(
            bus.clone(),
            Arc::new(tools::ToolExecutor::new(bus)),
            model.clone(),
        )
        .with_run_store(runtime::RunStore::open(&storage_config, store.handle()).unwrap());
        let mut projects = StaticProjects::default();
        projects.insert(&ProjectId::new("project-1").unwrap(), root.clone());
        let host = Arc::new(Mutex::new(
            CommandHost::new(CommandService::new(
                runtime.clone(),
                tokio::runtime::Handle::current(),
            ))
            .with_projects(projects),
        ));
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
            host.clone(),
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
            runtime,
            model,
            host,
            root,
            identity,
            socket: instance.socket.clone(),
            _store: store,
            _dir: dir,
            _instance: instance,
            _stop: stop,
            _server: server,
        }
    }

    /// The CLI's connection, made the way `evorch run` makes it.
    async fn cli(&self) -> WorkbenchClient {
        let launcher = core_launcher::Launcher::with_socket(
            core_instance::ConfigIdentity::new(&self.identity).unwrap(),
            core_launcher::CoreBinary::explicit(&std::env::current_exe().unwrap()).unwrap(),
            run_client_config(),
            self.socket.clone(),
        )
        .unwrap();
        connect_core(&launcher).await.unwrap().client
    }

    async fn screen(&self) -> WorkbenchClient {
        WorkbenchClient::connect(
            UnixTransport::connect(&self.socket).await.unwrap(),
            ClientConfig::new(
                ClientId::new("desktop-test").unwrap(),
                BuildInfo {
                    name: "test".into(),
                    version: "1".into(),
                    revision: None,
                },
            ),
        )
        .await
        .unwrap()
    }

    fn args(&self, prompt: &str, no_wait: bool) -> RunArgs {
        RunArgs {
            project_root: self.root.clone(),
            role: InvocationRole::Orchestrator,
            prompt: prompt.into(),
            user_config_dir: None,
            thread: None,
            no_wait,
        }
    }

    /// A run on a thread the screen started: the CLI must never touch it.
    async fn screen_run(&self, screen: &WorkbenchClient) -> RunId {
        let reply = screen
            .command(Command::Conversation(ConversationCommand::SendChat(
                SendChat {
                    project: ProjectId::new("project-1").unwrap(),
                    thread: ThreadId::new("screen-thread").unwrap(),
                    submission_id: SubmissionId::new("screen-1").unwrap(),
                    composer_role: ComposerRole::Worker,
                    text: "hold the screen".into(),
                    skills: vec![],
                    attachments: vec![],
                    model_preference: None,
                    fork_seed: None,
                },
            )))
            .await
            .unwrap();
        self.reached().await;
        reply.run_id.unwrap().as_str().parse().unwrap()
    }

    /// A run reached the model (bounded, so a failing test ends).
    async fn reached(&self) {
        tokio::time::timeout(WAIT, self.model.reached.acquire())
            .await
            .expect("a run reached the model")
            .unwrap()
            .forget();
    }

    fn live(&self, run: RunId) -> bool {
        matches!(
            self.runtime.inspect_agent(run).map(|agent| agent.phase),
            Ok(AgentRunPhase::Pending | AgentRunPhase::Running | AgentRunPhase::Waiting)
        )
    }

    /// The runs that are not `known`: what the CLI started.
    fn other_runs(&self, known: Option<RunId>) -> Vec<RunId> {
        self.runtime
            .list_agents()
            .into_iter()
            .map(|agent| agent.run_id)
            .filter(|run| Some(*run) != known)
            .collect()
    }

    async fn stopped(&self, run: RunId) {
        let phase = tokio::time::timeout(WAIT, self.runtime.wait(run))
            .await
            .expect("the run settles")
            .expect("the run is known");
        assert_eq!(phase, AgentRunPhase::Stopped);
    }

    fn running_invocations(&self) -> usize {
        self.host
            .lock()
            .unwrap()
            .service()
            .restart_gate()
            .running_invocations()
    }
}

fn never() -> impl std::future::Future<Output = ()> {
    std::future::pending()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_run_answers_and_its_text_is_the_output() {
    let core = Core::start().await;
    let client = core.cli().await;

    let outcome = tokio::time::timeout(
        WAIT,
        execute(
            &client,
            &core.args("say it", false),
            &Typed::default(),
            never(),
        ),
    )
    .await
    .expect("the run ends")
    .unwrap();

    assert_eq!(outcome, Outcome::Succeeded(Some("all done".into())));
    assert_eq!(outcome.render().2, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_is_asked_on_the_terminal_and_the_answer_reaches_the_run() {
    let core = Core::start().await;
    let client = core.cli().await;
    let terminal = Typed::default();

    let outcome = tokio::time::timeout(
        WAIT,
        execute(&client, &core.args("ask me", false), &terminal, never()),
    )
    .await
    .expect("the run ends")
    .unwrap();

    assert_eq!(outcome, Outcome::Succeeded(Some("all done".into())));
    assert_eq!(*terminal.answers.lock().unwrap(), ["Which format?"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn without_waiting_a_question_ends_the_command_with_exit_3_and_stops_the_run() {
    let core = Core::start().await;
    let client = core.cli().await;
    let terminal = Typed::default();

    let outcome = tokio::time::timeout(
        WAIT,
        execute(&client, &core.args("ask me", true), &terminal, never()),
    )
    .await
    .expect("the command ends")
    .unwrap();

    assert!(matches!(outcome, Outcome::NeedsInput(_)), "{outcome:?}");
    assert_eq!(outcome.render().2, 3);
    assert!(
        terminal.answers.lock().unwrap().is_empty(),
        "no prompt without waiting"
    );
    let runs = core.other_runs(None);
    assert_eq!(runs.len(), 1);
    core.stopped(runs[0]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ctrl_c_stops_the_invocations_own_run_and_not_the_screens() {
    let core = Core::start().await;
    let screen = core.screen().await;
    let screen_run = core.screen_run(&screen).await;
    let client = core.cli().await;
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let args = core.args("hold the cli", false);
    let task = tokio::spawn(async move {
        execute(&client, &args, &Typed::default(), async {
            let _ = interrupted.await;
        })
        .await
    });
    core.reached().await;
    assert_eq!(core.running_invocations(), 1);

    interrupt.send(()).unwrap();
    let outcome = tokio::time::timeout(WAIT, task)
        .await
        .expect("the command ends")
        .unwrap()
        .unwrap();

    assert_eq!(outcome, Outcome::Interrupted);
    assert_eq!(outcome.render().2, 130);
    let runs = core.other_runs(Some(screen_run));
    assert_eq!(runs.len(), 1);
    core.stopped(runs[0]).await;
    assert!(core.live(screen_run), "the screen's run was left alone");
    assert_eq!(core.running_invocations(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_lost_connection_stops_the_invocations_own_run_and_nothing_else() {
    let core = Core::start().await;
    let screen = core.screen().await;
    let screen_run = core.screen_run(&screen).await;
    let client = core.cli().await;
    let args = core.args("hold the cli", false);
    let task = tokio::spawn(async move {
        let _ = execute(&client, &args, &Typed::default(), never()).await;
    });
    core.reached().await;
    let runs = core.other_runs(Some(screen_run));
    assert_eq!(runs.len(), 1);

    // The process ends: its task and its connection go with it.
    task.abort();
    let _ = task.await;

    core.stopped(runs[0]).await;
    assert!(core.live(screen_run), "the screen's run was left alone");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_restart_is_refused_while_a_cli_invocation_runs_and_not_for_a_screen_alone() {
    let core = Core::start().await;
    let gate = core.host.lock().unwrap().service().restart_gate();
    let screen = core.screen().await;
    let _ = &screen;
    // A connection alone (a screen, the management CLI) is no reason to refuse.
    gate.claim_restart_admission().unwrap();
    gate.release_restart_admission();

    let client = core.cli().await;
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let args = core.args("hold the cli", false);
    let task = tokio::spawn(async move {
        execute(&client, &args, &Typed::default(), async {
            let _ = interrupted.await;
        })
        .await
    });
    core.reached().await;

    let refusal = gate.claim_restart_admission().unwrap_err();
    assert_eq!(refusal.running, 1);

    interrupt.send(()).unwrap();
    tokio::time::timeout(WAIT, task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    gate.claim_restart_admission()
        .expect("nothing runs any more");
}

fn started_of(
    reply: workbench_protocol::commands::CommandReply,
) -> workbench_protocol::namespaces::execution::invocation::InvocationStarted {
    match reply.result.map(|result| *result) {
        Some(CommandResult::Execution(ExecutionResult::InvocationStarted(started))) => started,
        other => panic!("not an invocation start: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn an_invocation_that_ended_does_not_stop_the_run_a_screen_resumed() {
    let core = Core::start().await;
    let cli = core.cli().await;
    let screen = core.screen().await;
    let id = InvocationId::new("inv-natural").unwrap();
    let started = started_of(
        cli.command(Command::Execution(ExecutionCommand::StartInvocation(
            StartInvocation {
                invocation: id.clone(),
                project_root: core.root.to_string_lossy().into_owned(),
                role: InvocationRole::Orchestrator,
                prompt: "say it".into(),
                thread: None,
            },
        )))
        .await
        .unwrap(),
    );
    let run: RunId = started.run.as_str().parse().unwrap();
    // The run ends by itself; the invocation has not been ended yet.
    let phase = tokio::time::timeout(WAIT, core.runtime.wait(run))
        .await
        .expect("the run settles")
        .unwrap();
    assert_eq!(phase, AgentRunPhase::Done);

    // A screen continues the same conversation: the same run id, a new execution.
    let resumed = screen
        .command(Command::Conversation(ConversationCommand::SendChat(
            SendChat {
                project: ProjectId::new("project-1").unwrap(),
                thread: started.thread.clone(),
                submission_id: SubmissionId::new("screen-resume").unwrap(),
                composer_role: ComposerRole::Worker,
                text: "hold the screen".into(),
                skills: vec![],
                attachments: vec![],
                model_preference: None,
                fork_seed: None,
            },
        )))
        .await
        .unwrap();
    core.reached().await;
    assert_eq!(resumed.run_id.as_ref(), Some(&started.run));
    assert!(core.live(run));

    // The old invocation ends now: it must not stop what it did not start.
    let ended = match cli
        .command(Command::Execution(ExecutionCommand::EndInvocation(
            EndInvocation {
                invocation: id,
                reason: EndReason::Interrupted,
            },
        )))
        .await
        .unwrap()
        .result
        .map(|result| *result)
    {
        Some(CommandResult::Execution(ExecutionResult::InvocationEnded(ended))) => ended,
        other => panic!("not an invocation end: {other:?}"),
    };

    assert_eq!(ended.stopped_runs, 0);
    assert!(
        core.live(run),
        "the resumed execution is not the invocation's"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_question_answered_on_a_screen_ends_the_wait_for_the_terminal() {
    let core = Core::start().await;
    let client = core.cli().await;
    let screen = core.screen().await;
    let terminal = Arc::new(Stuck::default());
    let args = core.args("ask me", false);
    let waiting = terminal.clone();
    let task = tokio::spawn(async move { execute(&client, &args, &*waiting, never()).await });
    tokio::time::timeout(WAIT, terminal.asked.notified())
        .await
        .expect("the terminal was asked");

    // The terminal is still waiting when a screen answers.
    let QueryReply::Questions(QuestionsReply::Questions(page)) = screen
        .query(Query::Questions(QuestionsQuery::ListQuestions(
            PendingQuery {
                thread: None,
                pending_only: true,
                page: Default::default(),
            },
        )))
        .await
        .unwrap()
    else {
        panic!("not a question list");
    };
    let question = page.items.first().expect("the run asked");
    screen
        .command(Command::Questions(QuestionsCommand::AnswerUserQuestion(
            AnswerUserQuestion {
                thread: question.requester.thread.clone(),
                question_id: question.id.clone(),
                answer: "YAML".into(),
                question_revision: question.revision,
            },
        )))
        .await
        .unwrap();

    let outcome = tokio::time::timeout(WAIT, task)
        .await
        .expect("the command ends although the terminal never answered")
        .unwrap()
        .unwrap();
    assert_eq!(outcome, Outcome::Succeeded(Some("all done".into())));
}

async fn approval_core(
    thread: &str,
    input: serde_json::Value,
) -> (
    Core,
    sandbox::approval::PendingApproval,
    Arc<sandbox::approval::ApprovalRegistry>,
    RunArgs,
) {
    let core = Core::start().await;
    let registry = sandbox::approval::ApprovalRegistry::for_bus(&core.runtime.event_bus());
    let mut args = core.args("hold the cli", false);
    args.thread = Some(ThreadId::new(thread).unwrap());
    let ticket = registry.register(
        sandbox::approval::ApprovalRequest {
            run_id: "run-1",
            thread_id: Some(thread),
            tool_name: "shell",
            call_id: "call-1",
            input: Some(input),
        },
        Duration::from_secs(3600),
    );
    (core, ticket, registry, args)
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_cut_short_is_approved_only_after_its_complete_input_was_shown() {
    let input = serde_json::json!({"command": "あ".repeat(35_000)});
    let (core, ticket, _registry, args) = approval_core("approval-whole", input.clone()).await;
    let client = core.cli().await;
    let terminal = Arc::new(Approver::new(Decision::Approve));
    let running = terminal.clone();
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        execute(&client, &args, &*running, async {
            let _ = interrupted.await;
        })
        .await
    });

    let state = tokio::time::timeout(WAIT, ticket.wait())
        .await
        .expect("the approval is decided");

    assert_eq!(state, sandbox::approval::ApprovalState::Approved);
    assert_eq!(
        *terminal.seen.lock().unwrap(),
        [(true, Some(input.to_string().len()))],
        "the terminal was shown the whole input"
    );
    interrupt.send(()).unwrap();
    let _ = tokio::time::timeout(WAIT, task)
        .await
        .expect("the command ends");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_approval_whose_complete_input_cannot_be_shown_is_not_approved_from_the_terminal() {
    // Larger than the terminal shows: it can only be read, and approved, on a screen.
    let input = serde_json::json!({"command": "a".repeat(300_000)});
    let (core, ticket, registry, args) = approval_core("approval-large", input).await;
    let client = core.cli().await;
    // The person types "y".
    let terminal = Arc::new(Approver::new(Decision::Approve));
    let running = terminal.clone();
    let (interrupt, interrupted) = tokio::sync::oneshot::channel::<()>();
    let task = tokio::spawn(async move {
        execute(&client, &args, &*running, async {
            let _ = interrupted.await;
        })
        .await
    });
    tokio::time::timeout(WAIT, terminal.shown.notified())
        .await
        .expect("the terminal was asked");

    interrupt.send(()).unwrap();
    let outcome = tokio::time::timeout(WAIT, task)
        .await
        .expect("the command ends")
        .unwrap()
        .unwrap();

    assert_eq!(outcome, Outcome::Interrupted);
    assert_eq!(*terminal.seen.lock().unwrap(), [(true, None)]);
    assert_eq!(
        registry.get(ticket.id()).unwrap().state,
        sandbox::approval::ApprovalState::Pending,
        "nothing was approved"
    );
}
