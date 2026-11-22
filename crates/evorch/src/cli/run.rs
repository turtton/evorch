//! `evorch run ...`: one agent run, as a client of the core (ADR 0045 §10).
//!
//! The command line neither opens the configuration database nor builds a
//! runtime: it meets the core of its configuration (starting it when none runs),
//! asks it to start an *invocation* and follows that invocation. There is no
//! embedded fallback: a core that cannot be reached is the command's failure.
//!
//! What the invocation owns is the run it started and that run's subtree, and
//! nothing else. Ctrl-C, a lost connection and the end of this process stop
//! that, and withdraw or detach the input the invocation sent to runs it does
//! not own (the core does this, see `workbench_host::invocations`). A command
//! whose outcome is unknown is never sent again.
//!
//! Exit codes: 0 the run (or the delivery of its input) succeeded, 1 it failed,
//! was refused or its outcome is unknown, 3 it needs an answer and `--no-wait`
//! was given (or no answer could be read), 130 interrupted.
use std::collections::HashSet;
use std::future::Future;
use std::io::BufRead;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use workbench_client::{ClientError, WorkbenchClient};
use workbench_protocol::{
    commands::{AnswerUserQuestion, ApprovalDecision, Command, CommandResult, DecideToolApproval},
    errors::ErrorCode,
    events::SubscriptionScope,
    handshake::{Capability, LocalSurface},
    ids::{InvocationId, ThreadId},
    namespaces::{
        ApprovalsCommand, ApprovalsQuery, ApprovalsReply, ExecutionCommand, ExecutionQuery,
        ExecutionReply, QuestionsCommand, QuestionsQuery, QuestionsReply,
        execution::{
            ExecutionResult,
            invocation::{
                EndInvocation, EndReason, GetInvocation, InvocationOwnership, InvocationRole,
                InvocationState, InvocationView, StartInvocation,
            },
        },
    },
    queries::{GetApprovalInput, PendingQuery, Query, QueryReply},
    views::{ApprovalState, ApprovalView, QuestionTarget, QuestionView},
    wire::ByteOffset,
};

use crate::cli::core_client::{self, CliError, Step, exit};

pub const USAGE: &str = "usage: evorch run --project <dir> --role <worker|orchestrator|explorer|reviewer|web_researcher|planner|advisor> --prompt <text> [--thread <id>] [--no-wait] [--user-config <dir>]";

/// How long the invocation goes without being read when no event came: the
/// events wake the command up, they are not the only way it learns the outcome.
const POLL: Duration = Duration::from_secs(1);

/// How long Ctrl-C waits for the core to confirm that the invocation ended.
const END_TIMEOUT: Duration = Duration::from_secs(10);

/// The largest complete input of a tool call that is shown on the terminal to
/// be approved. A larger one is read on a screen; the terminal cannot approve it.
const TERMINAL_INPUT_LIMIT: u64 = 256 * 1024;
const INPUT_CHUNK: u32 = 32 * 1024;

/// `evorch run`, as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunArgs {
    pub project_root: PathBuf,
    pub role: InvocationRole,
    pub prompt: String,
    pub user_config_dir: Option<PathBuf>,
    /// Continue this thread instead of starting a new one.
    pub thread: Option<ThreadId>,
    /// Do not wait for an answer: a run that needs one ends the command (exit 3).
    pub no_wait: bool,
}

fn parse_role(text: &str) -> Option<InvocationRole> {
    match text.to_ascii_lowercase().as_str() {
        "orchestrator" => Some(InvocationRole::Orchestrator),
        "explorer" => Some(InvocationRole::Explorer),
        "worker" => Some(InvocationRole::Worker),
        "reviewer" => Some(InvocationRole::Reviewer),
        "web_researcher" | "webresearcher" => Some(InvocationRole::WebResearcher),
        "planner" => Some(InvocationRole::Planner),
        "advisor" => Some(InvocationRole::Advisor),
        _ => None,
    }
}

/// The command line after the program name; `run` comes first.
///
/// # Errors
/// The text to print: the usage, or why an option is not accepted.
pub fn parse_args(argv: impl Iterator<Item = String>) -> Result<RunArgs, String> {
    let mut argv = argv;
    if argv.next().as_deref() != Some("run") {
        return Err(USAGE.into());
    }
    let (mut project, mut role, mut prompt) = (None, None, None);
    let (mut user_config, mut thread, mut no_wait) = (None, None, false);
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--no-wait" => no_wait = true,
            "--project" | "--role" | "--prompt" | "--user-config" | "--thread" => {
                let value = argv.next().ok_or_else(|| USAGE.to_owned())?;
                match flag.as_str() {
                    "--project" => project = Some(value),
                    "--role" => role = Some(value),
                    "--prompt" => prompt = Some(value),
                    "--user-config" => user_config = Some(value),
                    _ => thread = Some(value),
                }
            }
            "--db" => {
                return Err(
                    "--db is not accepted: the core owns the database (inspect it with `evorch inspect`)"
                        .into(),
                );
            }
            "--web-tools-enabled" | "--run-js-enabled" => {
                return Err(format!(
                    "{flag} is not accepted: the core owns the settings every client shares; change them in the settings"
                ));
            }
            _ => return Err(USAGE.into()),
        }
    }
    let (Some(project), Some(role), Some(prompt)) = (project, role, prompt) else {
        return Err(USAGE.into());
    };
    let role = parse_role(&role).ok_or_else(|| USAGE.to_owned())?;
    let project_root = std::path::absolute(&project).map_err(|_| USAGE.to_owned())?;
    let thread = thread
        .map(ThreadId::new)
        .transpose()
        .map_err(|error| format!("--thread: {error}"))?;
    Ok(RunArgs {
        project_root,
        role,
        prompt,
        user_config_dir: user_config.map(PathBuf::from),
        thread,
        no_wait,
    })
}

/// How an invocation ended, as far as the command is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The run finished; what it answered, if anything.
    Succeeded(Option<String>),
    /// The input reached a run this command does not own.
    Delivered,
    Failed(String),
    /// The run waits for an answer this command did not (or could not) give.
    NeedsInput(String),
    Interrupted,
}

impl Outcome {
    /// What the command prints (stdout, stderr) and how it exits.
    pub fn render(&self) -> (Option<String>, Option<String>, u8) {
        match self {
            Self::Succeeded(text) => (text.clone(), None, exit::SUCCESS),
            Self::Delivered => (
                None,
                Some("the input reached the run".into()),
                exit::SUCCESS,
            ),
            Self::Failed(detail) => (None, Some(detail.clone()), exit::FAILURE),
            Self::NeedsInput(what) => (
                None,
                Some(format!(
                    "waiting for input: {what}; the run was stopped (answer it from a screen, or run again without --no-wait)"
                )),
                exit::WAITING_INPUT,
            ),
            Self::Interrupted => (None, Some("interrupted".into()), exit::INTERRUPTED),
        }
    }
}

/// What the person decided about a tool approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
    /// No decision here: the approval stays pending for a screen.
    Leave,
}

/// Where answers come from. `None` / `Leave` mean that none could be given.
#[async_trait]
pub trait Prompter: Send + Sync {
    async fn answer(&self, question: &QuestionView) -> Option<String>;
    /// `input` is the complete input of the call (secrets masked) when it was
    /// fetched. A call whose summary was cut short and whose complete input is
    /// not at hand is never approved from here, whatever is answered.
    async fn approve(&self, approval: &ApprovalView, input: Option<&str>) -> Decision;
}

/// The terminal: prompts on stderr, answers from stdin.
pub struct Terminal;

/// stdin is read by one thread for the whole process, so that a prompt that is
/// given up (answered on a screen) leaves nothing blocking behind it but that
/// thread, which does not keep the process from ending.
fn stdin_lines() -> &'static tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Option<String>>>
{
    static LINES: std::sync::OnceLock<
        tokio::sync::Mutex<tokio::sync::mpsc::UnboundedReceiver<Option<String>>>,
    > = std::sync::OnceLock::new();
    LINES.get_or_init(|| {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let _ = std::thread::Builder::new()
            .name("evorch-stdin".into())
            .spawn(move || {
                let stdin = std::io::stdin();
                loop {
                    let mut line = String::new();
                    match stdin.lock().read_line(&mut line) {
                        Ok(0) | Err(_) => {
                            let _ = tx.send(None);
                            break;
                        }
                        Ok(_) => {
                            if tx.send(Some(line.trim().to_owned())).is_err() {
                                break;
                            }
                        }
                    }
                }
            });
        tokio::sync::Mutex::new(rx)
    })
}

/// Set when a prompt was given up: what was typed for it is not an answer to
/// the next one.
static PROMPT_GIVEN_UP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

struct GiveUp(bool);

impl Drop for GiveUp {
    fn drop(&mut self) {
        if self.0 {
            PROMPT_GIVEN_UP.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
}

async fn read_line() -> Option<String> {
    let mut lines = stdin_lines().lock().await;
    if PROMPT_GIVEN_UP.swap(false, std::sync::atomic::Ordering::SeqCst) {
        while let Ok(Some(_)) = lines.try_recv() {}
    }
    let mut guard = GiveUp(true);
    let line = lines.recv().await.flatten();
    guard.0 = false;
    line
}

/// What the terminal shows for an approval, and whether it may be approved there.
fn approval_text(approval: &ApprovalView, input: Option<&str>) -> (String, bool) {
    match (approval.summary_truncated, input) {
        (false, _) => (approval.summary.clone(), true),
        (true, Some(input)) => (input.to_owned(), true),
        (true, None) => (
            format!(
                "{}\n[the summary is cut short and the complete input could not be shown: it cannot be approved from this terminal; use a screen]",
                approval.summary
            ),
            false,
        ),
    }
}

/// The decision a typed line stands for. Only an approval whose complete input
/// was shown can be approved; the rest can be denied or left for a screen.
fn decision_of(approvable: bool, line: Option<&str>) -> Decision {
    let line = line.map(str::to_ascii_lowercase);
    match (approvable, line.as_deref()) {
        (true, Some("y" | "yes")) => Decision::Approve,
        (true, _) => Decision::Deny,
        (false, Some("d" | "deny")) => Decision::Deny,
        (false, _) => Decision::Leave,
    }
}

#[async_trait]
impl Prompter for Terminal {
    async fn answer(&self, question: &QuestionView) -> Option<String> {
        eprintln!("{}", question.title);
        for (n, option) in question.options.iter().enumerate() {
            eprintln!("  {}) {option}", n + 1);
        }
        eprint!("> ");
        let line = read_line().await?;
        Some(
            line.parse::<usize>()
                .ok()
                .and_then(|n| n.checked_sub(1))
                .and_then(|n| question.options.get(n))
                .cloned()
                .unwrap_or(line),
        )
    }

    async fn approve(&self, approval: &ApprovalView, input: Option<&str>) -> Decision {
        let (text, approvable) = approval_text(approval, input);
        eprintln!("{text}");
        if approvable {
            eprint!("Approve {} [{}]? [y/N] ", approval.tool_name, approval.id);
        } else {
            eprint!(
                "Deny {} [{}] here? [d = deny, Enter = leave it for a screen] ",
                approval.tool_name, approval.id
            );
        }
        decision_of(approvable, read_line().await.as_deref())
    }
}

fn execution_query<V>(
    pick: impl FnOnce(ExecutionReply) -> Option<V>,
) -> impl FnOnce(QueryReply) -> Option<V> {
    move |reply| match reply {
        QueryReply::Execution(reply) => pick(reply),
        _ => None,
    }
}

async fn view_of(
    client: &WorkbenchClient,
    id: &InvocationId,
) -> Result<Option<InvocationView>, ClientError> {
    core_client::read_operation(
        client,
        Query::Execution(ExecutionQuery::GetInvocation(GetInvocation {
            invocation: id.clone(),
        })),
        execution_query(|reply| match reply {
            ExecutionReply::Invocation(view) => Some(*view),
            _ => None,
        }),
    )
    .await
}

/// Questions and approvals of `thread` that wait for an answer, and that the
/// run `owned` (or its subtree) raised.
async fn pending(
    client: &WorkbenchClient,
    thread: &ThreadId,
    owned: &workbench_protocol::ids::RunId,
) -> Result<(Vec<QuestionView>, Vec<ApprovalView>), ClientError> {
    let query = PendingQuery {
        thread: Some(thread.clone()),
        pending_only: true,
        page: Default::default(),
    };
    let QueryReply::Questions(QuestionsReply::Questions(questions)) = client
        .query(Query::Questions(QuestionsQuery::ListQuestions(
            query.clone(),
        )))
        .await?
    else {
        return Ok((vec![], vec![]));
    };
    let QueryReply::Approvals(ApprovalsReply::Approvals(approvals)) = client
        .query(Query::Approvals(ApprovalsQuery::ListApprovals(query)))
        .await?
    else {
        return Ok((vec![], vec![]));
    };
    let questions = questions
        .items
        .into_iter()
        .filter(|q| {
            q.blocking
                && q.answer.is_none()
                && q.target == QuestionTarget::User
                && &q.requester.root_run == owned
        })
        .collect();
    let approvals = approvals
        .items
        .into_iter()
        .filter(|a| a.state == ApprovalState::Pending)
        .collect();
    Ok((questions, approvals))
}

fn refused(error: &ClientError) -> bool {
    matches!(error, ClientError::Server(e) if e.code != ErrorCode::StorageUnavailable)
}

/// The complete input of a call whose summary was cut short, read in chunks
/// and checked to be whole. `None` when it cannot be had (too large for the
/// terminal, changed or decided meanwhile, or not consistent).
async fn complete_input(
    client: &WorkbenchClient,
    approval: &ApprovalView,
) -> Result<Option<String>, ClientError> {
    let mut text = String::new();
    loop {
        let reply = client
            .query(Query::Approvals(ApprovalsQuery::GetApprovalInput(
                GetApprovalInput {
                    approval_id: approval.id.clone(),
                    core_epoch: approval.core_epoch,
                    approval_revision: approval.revision,
                    offset: ByteOffset(text.len() as u64),
                    max_bytes: INPUT_CHUNK,
                },
            )))
            .await;
        let chunk = match reply {
            Ok(QueryReply::Approvals(ApprovalsReply::ApprovalInput(chunk))) => chunk,
            Ok(_) => return Ok(None),
            Err(error) if refused(&error) => return Ok(None),
            Err(error) => return Err(error),
        };
        if chunk.approval_id != approval.id
            || chunk.core_epoch != approval.core_epoch
            || chunk.approval_revision != approval.revision
            || chunk.offset.get() != text.len() as u64
            || chunk.total_bytes.get() > TERMINAL_INPUT_LIMIT
        {
            return Ok(None);
        }
        text.push_str(&chunk.text);
        match chunk.next_offset {
            None => {
                return Ok((text.len() as u64 == chunk.total_bytes.get()).then_some(text));
            }
            Some(next) if next.get() == text.len() as u64 && !chunk.text.is_empty() => {}
            Some(_) => return Ok(None),
        }
    }
}

/// One thing that waits for the person.
enum Waiting {
    Question(QuestionView),
    Approval(ApprovalView),
}

impl Waiting {
    fn key(&self) -> String {
        match self {
            Self::Question(q) => format!("q:{}:{:?}", q.id, q.revision),
            Self::Approval(a) => format!("a:{}:{:?}", a.id, a.revision),
        }
    }
}

/// What waits for the person. Reading it ([`Asking::observe`]) never waits for
/// the person: what is new is handed to [`Asking::serve`], which prompts while
/// the invocation keeps being followed, and gives a prompt up as soon as its
/// question or approval is no longer pending (answered on a screen).
struct Asking<'a> {
    client: &'a WorkbenchClient,
    prompter: &'a dyn Prompter,
    no_wait: bool,
    /// What was handed over already, by id and revision: never asked twice.
    shown: Mutex<HashSet<String>>,
    /// What is pending now.
    live: tokio::sync::watch::Sender<HashSet<String>>,
    work: tokio::sync::mpsc::UnboundedSender<Waiting>,
}

impl<'a> Asking<'a> {
    fn new(
        client: &'a WorkbenchClient,
        prompter: &'a dyn Prompter,
        no_wait: bool,
    ) -> (Self, tokio::sync::mpsc::UnboundedReceiver<Waiting>) {
        let (work, queue) = tokio::sync::mpsc::unbounded_channel();
        let (live, _) = tokio::sync::watch::channel(HashSet::new());
        (
            Self {
                client,
                prompter,
                no_wait,
                shown: Mutex::default(),
                live,
                work,
            },
            queue,
        )
    }

    fn first_time(&self, key: String) -> bool {
        self.shown.lock().unwrap().insert(key)
    }

    /// Read what is pending. `Some` ends the wait.
    async fn observe(
        &self,
        view: &InvocationView,
        run: &workbench_protocol::ids::RunId,
    ) -> Result<Option<Outcome>, ClientError> {
        let (questions, approvals) = pending(self.client, &view.thread, run).await?;
        let waiting: Vec<Waiting> = questions
            .into_iter()
            .map(Waiting::Question)
            .chain(approvals.into_iter().map(Waiting::Approval))
            .collect();
        self.live
            .send_replace(waiting.iter().map(Waiting::key).collect());
        if waiting.is_empty() {
            return Ok(None);
        }
        if self.no_wait {
            let what = match &waiting[0] {
                Waiting::Question(q) => format!("question: {}", q.title),
                Waiting::Approval(a) => format!("approval of {}", a.tool_name),
            };
            return Ok(Some(Outcome::NeedsInput(what)));
        }
        for item in waiting {
            if self.first_time(item.key()) {
                let _ = self.work.send(item);
            }
        }
        Ok(None)
    }

    /// Prompt for what [`Self::observe`] hands over. Ends with `Some` outcome
    /// when the command cannot go on; otherwise it never ends by itself.
    async fn serve(
        &self,
        mut queue: tokio::sync::mpsc::UnboundedReceiver<Waiting>,
    ) -> Result<Outcome, ClientError> {
        while let Some(item) = queue.recv().await {
            let key = item.key();
            let mut live = self.live.subscribe();
            let gone = async {
                let _ = live.wait_for(|pending| !pending.contains(&key)).await;
            };
            let outcome = tokio::select! {
                () = gone => None,
                outcome = self.prompt(&item) => outcome?,
            };
            if let Some(outcome) = outcome {
                return Ok(outcome);
            }
        }
        std::future::pending().await
    }

    async fn prompt(&self, item: &Waiting) -> Result<Option<Outcome>, ClientError> {
        match item {
            Waiting::Question(question) => self.ask_question(question).await,
            Waiting::Approval(approval) => self.ask_approval(approval).await,
        }
    }

    async fn ask_question(&self, question: &QuestionView) -> Result<Option<Outcome>, ClientError> {
        let Some(answer) = self.prompter.answer(question).await else {
            return Ok(Some(Outcome::NeedsInput(format!(
                "question: {} (no answer could be read)",
                question.title
            ))));
        };
        let sent = self
            .client
            .command(Command::Questions(QuestionsCommand::AnswerUserQuestion(
                AnswerUserQuestion {
                    thread: question.requester.thread.clone(),
                    question_id: question.id.clone(),
                    answer,
                    question_revision: question.revision,
                },
            )))
            .await;
        self.sent("answer", sent)
    }

    async fn ask_approval(&self, approval: &ApprovalView) -> Result<Option<Outcome>, ClientError> {
        let input = if approval.summary_truncated {
            complete_input(self.client, approval).await?
        } else {
            None
        };
        let mut decision = self.prompter.approve(approval, input.as_deref()).await;
        if decision == Decision::Approve && approval.summary_truncated && input.is_none() {
            // Whatever the prompter says: what was not shown is not approved.
            eprintln!(
                "the complete input of {} could not be shown; it is not approved from this terminal",
                approval.tool_name
            );
            decision = Decision::Leave;
        }
        let decision = match decision {
            Decision::Approve => ApprovalDecision::Approve,
            Decision::Deny => ApprovalDecision::Deny,
            Decision::Leave => return Ok(None),
        };
        let sent = self
            .client
            .command(Command::Approvals(ApprovalsCommand::DecideToolApproval(
                DecideToolApproval {
                    approval_id: approval.id.clone(),
                    core_epoch: approval.core_epoch,
                    approval_revision: approval.revision,
                    decision,
                },
            )))
            .await;
        self.sent("decision", sent)
    }

    /// A refusal because it was decided or changed elsewhere in the meantime is
    /// not a failure: the next read tells what became of it.
    fn sent<T>(
        &self,
        what: &str,
        sent: Result<T, ClientError>,
    ) -> Result<Option<Outcome>, ClientError> {
        match sent {
            Ok(_) => Ok(None),
            Err(ClientError::Server(e))
                if matches!(
                    e.code,
                    ErrorCode::AlreadyResolved | ErrorCode::StaleRevision | ErrorCode::StaleEpoch
                ) =>
            {
                Ok(None)
            }
            Err(error) if refused(&error) => Ok(Some(Outcome::Failed(format!(
                "the core refused the {what}: {error}"
            )))),
            Err(error) => Err(error),
        }
    }
}

fn settle(view: &InvocationView) -> Option<Outcome> {
    let detail = |fallback: &str| {
        view.detail
            .clone()
            .or_else(|| view.final_text.clone())
            .unwrap_or_else(|| fallback.to_owned())
    };
    match view.state {
        InvocationState::Running | InvocationState::WaitingForInput => None,
        InvocationState::Succeeded if view.owned_run.is_some() => {
            Some(Outcome::Succeeded(view.final_text.clone()))
        }
        InvocationState::Succeeded => Some(Outcome::Delivered),
        InvocationState::Failed => Some(Outcome::Failed(detail("the run failed"))),
        InvocationState::Stopped => Some(Outcome::Failed(
            "the run was stopped by someone else".into(),
        )),
        InvocationState::Ended => Some(Outcome::Failed("the invocation ended".into())),
    }
}

/// Ask the core to end the invocation: it stops the run it owns (and that run's
/// subtree), withdraws or detaches the input it sent. Best effort: when the
/// connection is gone, the core does the same by itself.
async fn end(client: &WorkbenchClient, id: &InvocationId, reason: EndReason) {
    let command = Command::Execution(ExecutionCommand::EndInvocation(EndInvocation {
        invocation: id.clone(),
        reason,
    }));
    let _ = tokio::time::timeout(END_TIMEOUT, client.command(command)).await;
}

async fn follow(
    client: &WorkbenchClient,
    args: &RunArgs,
    prompter: &dyn Prompter,
    id: &InvocationId,
) -> Result<Outcome, CliError> {
    let reply = core_client::send(
        client,
        Command::Execution(ExecutionCommand::StartInvocation(StartInvocation {
            invocation: id.clone(),
            project_root: args.project_root.to_string_lossy().into_owned(),
            role: args.role,
            prompt: args.prompt.clone(),
            thread: args.thread.clone(),
        })),
    )
    .await?;
    let Some(CommandResult::Execution(ExecutionResult::InvocationStarted(started))) =
        reply.result.as_deref()
    else {
        return Err(CliError::Failed(
            "protocol violation: the core did not answer with an invocation".into(),
        ));
    };
    let (asking, queue) = Asking::new(client, prompter, args.no_wait);
    let owns = started.ownership == InvocationOwnership::OwnsRun;
    let waiting = core_client::wait_settled_every(
        client,
        SubscriptionScope::All,
        |_| true,
        Some(POLL),
        || async {
            let Some(view) = view_of(client, id).await? else {
                return Ok(Step::Unknown);
            };
            if let Some(outcome) = settle(&view) {
                return Ok(Step::Settled(outcome));
            }
            if owns
                && let Some(run) = &view.owned_run
                && let Some(outcome) = asking.observe(&view, run).await?
            {
                return Ok(Step::Settled(outcome));
            }
            Ok(Step::Pending)
        },
    );
    // The person is asked while the invocation is followed: an end, a loss of
    // the connection or an answer given elsewhere ends the wait for the terminal.
    tokio::select! {
        result = waiting => result,
        served = asking.serve(queue) => served.map_err(CliError::from),
    }
}

/// Run one invocation to its end. `interrupt` completes when the person asks to
/// stop (Ctrl-C).
///
/// # Errors
/// The core could not be asked, or the outcome is unknown; nothing was sent twice.
pub async fn execute(
    client: &WorkbenchClient,
    args: &RunArgs,
    prompter: &dyn Prompter,
    interrupt: impl Future<Output = ()>,
) -> Result<Outcome, CliError> {
    let id = InvocationId::new(format!("run-{}", uuid::Uuid::new_v4()))
        .map_err(|error| CliError::Failed(error.to_string()))?;
    tokio::pin!(interrupt);
    let result = tokio::select! {
        result = follow(client, args, prompter, &id) => result,
        () = &mut interrupt => {
            end(client, &id, EndReason::Interrupted).await;
            return Ok(Outcome::Interrupted);
        }
    };
    if !matches!(result, Ok(Outcome::Succeeded(_) | Outcome::Delivered)) {
        end(client, &id, EndReason::Interrupted).await;
    }
    result
}

fn report(result: Result<Outcome, CliError>) -> ExitCode {
    match result {
        Ok(outcome) => {
            let (out, err, code) = outcome.render();
            if let Some(out) = out {
                println!("{out}");
            }
            if let Some(err) = err {
                eprintln!("{err}");
            }
            exit::code(code)
        }
        Err(error) => {
            eprintln!("{error}");
            error.exit_code()
        }
    }
}

/// The client `evorch run` is: the command line, which the core records as the
/// surface of what it starts, and one that can follow events and answer.
pub fn run_client_config() -> workbench_client::ClientConfig {
    let mut config = core_client::client_config(&[
        Capability::SnapshotEvents,
        Capability::Questions,
        Capability::Approvals,
    ]);
    config.local_surface = Some(LocalSurface::Cli);
    config
}

async fn run(args: RunArgs) -> ExitCode {
    let config = run_client_config();
    let connected = match core_client::launcher_with(args.user_config_dir.clone(), config) {
        Ok(launcher) => core_client::connect_core(&launcher).await,
        Err(error) => Err(error),
    };
    let connected = match connected {
        Ok(connected) => connected,
        Err(error) => return report(Err(error)),
    };
    report(
        execute(&connected.client, &args, &Terminal, async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await,
    )
}

pub fn main(argv: Vec<String>) -> ExitCode {
    let args = match parse_args(argv.into_iter()) {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            return exit::code(exit::FAILURE);
        }
    };
    let runtime = match super::async_runtime() {
        Ok(runtime) => runtime,
        Err(code) => return code,
    };
    let code = runtime.block_on(run(args));
    // A prompt still waiting on stdin must not hold the process after Ctrl-C.
    runtime.shutdown_background();
    code
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<RunArgs, String> {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn a_full_command_line_is_parsed() {
        let args = parse(&[
            "run",
            "--project",
            "/work",
            "--role",
            "Reviewer",
            "--prompt",
            "look",
            "--thread",
            "thread-1",
            "--no-wait",
            "--user-config",
            "cfg",
        ])
        .unwrap();
        assert_eq!(args.project_root, PathBuf::from("/work"));
        assert_eq!(args.role, InvocationRole::Reviewer);
        assert_eq!(args.thread.unwrap().as_str(), "thread-1");
        assert!(args.no_wait);
        assert_eq!(args.user_config_dir, Some(PathBuf::from("cfg")));
    }

    #[test]
    fn every_role_is_accepted() {
        for role in [
            "orchestrator",
            "explorer",
            "worker",
            "reviewer",
            "web_researcher",
            "planner",
            "advisor",
        ] {
            parse(&["run", "--project", "/p", "--role", role, "--prompt", "x"]).unwrap();
        }
        assert_eq!(
            parse(&[
                "run",
                "--project",
                "/p",
                "--role",
                "nobody",
                "--prompt",
                "x"
            ]),
            Err(USAGE.into())
        );
    }

    #[test]
    fn options_the_core_owns_are_refused_with_the_reason() {
        for (flag, value) in [
            ("--db", "x.db"),
            ("--web-tools-enabled", "true"),
            ("--run-js-enabled", "false"),
        ] {
            let error = parse(&[
                "run",
                "--project",
                "/p",
                "--role",
                "worker",
                "--prompt",
                "x",
                flag,
                value,
            ])
            .unwrap_err();
            assert!(
                error.contains(flag) && error.contains("core owns"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_missing_part_is_the_usage() {
        assert_eq!(parse(&["run", "--project", "/p"]), Err(USAGE.into()));
        assert_eq!(parse(&["--prompt", "x"]), Err(USAGE.into()));
        assert_eq!(
            parse(&["run", "--project", "/p", "--role", "worker", "--prompt"]),
            Err(USAGE.into())
        );
    }

    #[test]
    fn y_approves_only_what_was_shown_whole() {
        assert_eq!(decision_of(true, Some("y")), Decision::Approve);
        assert_eq!(decision_of(true, Some("YES")), Decision::Approve);
        assert_eq!(decision_of(true, Some("n")), Decision::Deny);
        assert_eq!(decision_of(true, None), Decision::Deny);
        // The summary was cut short and the complete input is not at hand.
        assert_eq!(decision_of(false, Some("y")), Decision::Leave);
        assert_eq!(decision_of(false, Some("yes")), Decision::Leave);
        assert_eq!(decision_of(false, None), Decision::Leave);
        assert_eq!(decision_of(false, Some("d")), Decision::Deny);
    }

    #[test]
    fn each_outcome_has_its_exit_code() {
        let codes = [
            (Outcome::Succeeded(Some("done".into())), 0),
            (Outcome::Delivered, 0),
            (Outcome::Failed("no".into()), 1),
            (Outcome::NeedsInput("q".into()), 3),
            (Outcome::Interrupted, 130),
        ];
        for (outcome, code) in codes {
            assert_eq!(outcome.render().2, code, "{outcome:?}");
        }
        assert_eq!(
            Outcome::Succeeded(Some("done".into()))
                .render()
                .0
                .as_deref(),
            Some("done")
        );
    }
}
