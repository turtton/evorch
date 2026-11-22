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
    queries::{PendingQuery, Query, QueryReply},
    views::{ApprovalState, ApprovalView, QuestionTarget, QuestionView},
};

use crate::cli::core_client::{self, CliError, Step, exit};

pub const USAGE: &str = "usage: evorch run --project <dir> --role <worker|orchestrator|explorer|reviewer|web_researcher|planner|advisor> --prompt <text> [--thread <id>] [--no-wait] [--user-config <dir>]";

/// How long the invocation goes without being read when no event came: the
/// events wake the command up, they are not the only way it learns the outcome.
const POLL: Duration = Duration::from_secs(1);

/// How long Ctrl-C waits for the core to confirm that the invocation ended.
const END_TIMEOUT: Duration = Duration::from_secs(10);

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

/// Where answers come from. `None` / `false` mean that none could be read.
#[async_trait]
pub trait Prompter: Send + Sync {
    async fn answer(&self, question: &QuestionView) -> Option<String>;
    async fn approve(&self, approval: &ApprovalView) -> bool;
}

/// The terminal: prompts on stderr, answers from stdin.
pub struct Terminal;

async fn read_line() -> Option<String> {
    tokio::task::spawn_blocking(|| {
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line.trim().to_owned()),
        }
    })
    .await
    .ok()
    .flatten()
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

    async fn approve(&self, approval: &ApprovalView) -> bool {
        eprintln!("{}", approval.summary);
        eprint!("Approve {} [{}]? [y/N] ", approval.tool_name, approval.id);
        matches!(
            read_line()
                .await
                .map(|line| line.to_ascii_lowercase())
                .as_deref(),
            Some("y" | "yes")
        )
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

/// Asks what waits for the person, once each. `Some` ends the wait.
struct Asking<'a> {
    client: &'a WorkbenchClient,
    prompter: &'a dyn Prompter,
    no_wait: bool,
    /// What was shown already, by id and revision: never asked twice.
    shown: Mutex<HashSet<String>>,
}

impl Asking<'_> {
    fn first_time(&self, key: String) -> bool {
        self.shown.lock().unwrap().insert(key)
    }

    async fn ask(
        &self,
        view: &InvocationView,
        run: &workbench_protocol::ids::RunId,
    ) -> Result<Option<Outcome>, ClientError> {
        let (questions, approvals) = pending(self.client, &view.thread, run).await?;
        if questions.is_empty() && approvals.is_empty() {
            return Ok(None);
        }
        if self.no_wait {
            let what = questions
                .first()
                .map(|q| format!("question: {}", q.title))
                .or_else(|| {
                    approvals
                        .first()
                        .map(|a| format!("approval of {}", a.tool_name))
                })
                .unwrap_or_default();
            return Ok(Some(Outcome::NeedsInput(what)));
        }
        for question in questions {
            if !self.first_time(format!("q:{}:{:?}", question.id, question.revision)) {
                continue;
            }
            let Some(answer) = self.prompter.answer(&question).await else {
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
            if let Err(error) = sent {
                // Answered elsewhere, or changed: the next read tells which.
                if !refused(&error) {
                    return Err(error);
                }
                return Ok(Some(Outcome::Failed(format!(
                    "the core refused the answer: {error}"
                ))));
            }
        }
        for approval in approvals {
            if !self.first_time(format!("a:{}:{:?}", approval.id, approval.revision)) {
                continue;
            }
            let approved = self.prompter.approve(&approval).await;
            let sent = self
                .client
                .command(Command::Approvals(ApprovalsCommand::DecideToolApproval(
                    DecideToolApproval {
                        approval_id: approval.id.clone(),
                        core_epoch: approval.core_epoch,
                        approval_revision: approval.revision,
                        decision: if approved {
                            ApprovalDecision::Approve
                        } else {
                            ApprovalDecision::Deny
                        },
                    },
                )))
                .await;
            if let Err(error) = sent {
                if !refused(&error) {
                    return Err(error);
                }
                return Ok(Some(Outcome::Failed(format!(
                    "the core refused the decision: {error}"
                ))));
            }
        }
        Ok(None)
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
    let asking = Asking {
        client,
        prompter,
        no_wait: args.no_wait,
        shown: Mutex::default(),
    };
    let owns = started.ownership == InvocationOwnership::OwnsRun;
    core_client::wait_settled_every(
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
            if owns && let Some(run) = &view.owned_run {
                if let Some(outcome) = asking.ask(&view, run).await? {
                    return Ok(Step::Settled(outcome));
                }
            }
            Ok(Step::Pending)
        },
    )
    .await
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
