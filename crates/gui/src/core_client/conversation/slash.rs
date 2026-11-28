//! The slash commands the core client carries out, with the embedded workbench's
//! wording. A command the core client has no way to carry (external commands) is not
//! offered and reads as unknown.
use workbench_protocol::commands::{
    Command, ComposerRole, ContinueChat, Delegation, SubmitGoal, ThreadTarget,
};
use workbench_protocol::ids::{ProjectId, ThreadId};
use workbench_protocol::namespaces::{ConversationCommand, ExecutionCommand};

use crate::composer_input::slash::{ComposerInput, help_text_among, parse_input};

/// The built-in commands offered here, in the embedded workbench's order.
pub(super) const OFFERED: &[&str] = &[
    "team",
    "goal",
    "help",
    "new",
    "compaction",
    "continue",
    "run",
    "btw",
];

pub(super) struct Ctx<'a> {
    pub project: &'a ProjectId,
    pub thread: &'a ThreadId,
    /// The core has the thread: a goal is submitted to it rather than creating it.
    pub exists: bool,
    /// The composer's role, which a continued conversation resumes as.
    pub role: ComposerRole,
}

#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    /// Not a command: the draft is sent as a message.
    Chat,
    /// Sent as is; the draft is cleared.
    Command(Box<Command>),
    /// Shown in the transcript; the draft is cleared.
    Help(String),
    /// Shown in the transcript; the draft is kept to be corrected.
    Usage(String),
    /// A new conversation in the project; the draft is cleared.
    NewConversation,
    /// A side question about the thread, asked without joining its conversation.
    Btw(String),
    /// A background Worker run in a new thread of the project, started without
    /// leaving the screen.
    Run(String),
}

pub(super) fn dispatch(text: &str, ctx: &Ctx<'_>) -> Outcome {
    let (spec, args) = match parse_input(text) {
        ComposerInput::Empty | ComposerInput::Chat(_) => return Outcome::Chat,
        ComposerInput::UnknownCommand { name } => return unknown(name),
        ComposerInput::Command { spec, args } => (spec, args),
    };
    match spec.name {
        "compaction" if !args.is_empty() => Outcome::Usage("usage: /compaction".into()),
        "compaction" if !ctx.exists => Outcome::Usage("Select or start a thread first".into()),
        "compaction" => Outcome::Command(Box::new(Command::Conversation(
            ConversationCommand::CompactChat(ThreadTarget {
                thread: ctx.thread.clone(),
            }),
        ))),
        "continue" if !args.is_empty() => Outcome::Usage("usage: /continue".into()),
        "continue" if !ctx.exists => Outcome::Usage("Select or start a thread first".into()),
        "continue" => Outcome::Command(Box::new(Command::Conversation(
            ConversationCommand::ContinueChat(ContinueChat {
                project: ctx.project.clone(),
                thread: ctx.thread.clone(),
                composer_role: ctx.role,
                model_preference: None,
            }),
        ))),
        "btw" if args.is_empty() => Outcome::Usage("usage: /btw <question>".into()),
        "btw" if !ctx.exists => Outcome::Usage("Select or start a thread first".into()),
        "btw" => Outcome::Btw(args.into()),
        "run" if args.is_empty() => Outcome::Usage("usage: /run <text>".into()),
        "run" => Outcome::Run(args.into()),
        "goal" if args.is_empty() => Outcome::Usage("usage: /goal <text>".into()),
        "goal" => goal(ctx, args, None),
        "team" => match args.split_once('|') {
            Some((value, objective))
                if !value.trim().is_empty() && !objective.trim().is_empty() =>
            {
                goal(
                    ctx,
                    objective.trim(),
                    Some(Delegation::Team {
                        value: value.trim().into(),
                    }),
                )
            }
            _ => Outcome::Usage("usage: /team <delegation value> | <goal>".into()),
        },
        "help" => Outcome::Help(help_text_among(Some(OFFERED))),
        "new" => Outcome::NewConversation,
        name => unknown(name),
    }
}

fn unknown(name: &str) -> Outcome {
    Outcome::Usage(format!("unknown command /{name} — type /help"))
}

fn goal(ctx: &Ctx<'_>, objective: &str, delegation: Option<Delegation>) -> Outcome {
    let goal = SubmitGoal {
        project: ctx.project.clone(),
        thread: ctx.thread.clone(),
        objective: objective.into(),
        references: vec![],
        constraints: vec![],
        delegation,
    };
    Outcome::Command(Box::new(if ctx.exists {
        Command::Execution(ExecutionCommand::SubmitGoal(goal))
    } else {
        Command::Conversation(ConversationCommand::CreateGoal(goal))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(text: &str, exists: bool) -> Outcome {
        dispatch(
            text,
            &Ctx {
                project: &ProjectId::new("p").unwrap(),
                thread: &ThreadId::new("t").unwrap(),
                exists,
                role: ComposerRole::Orchestrator,
            },
        )
    }

    fn goal_of(outcome: Outcome) -> (bool, SubmitGoal) {
        let Outcome::Command(command) = outcome else {
            panic!("not a command: {outcome:?}");
        };
        match *command {
            Command::Execution(ExecutionCommand::SubmitGoal(goal)) => (true, goal),
            Command::Conversation(ConversationCommand::CreateGoal(goal)) => (false, goal),
            other => panic!("not a goal: {other:?}"),
        }
    }

    // Given: /goal and /team on a thread the core has and on a new conversation
    // Then: the core's thread gets SubmitGoal and the new one CreateGoal; /team
    //       carries its delegation value, trimmed
    #[test]
    fn goal_and_team_submit_or_create_the_goal() {
        for exists in [true, false] {
            let (submitted, goal) = goal_of(run("/goal  ship it ", exists));
            assert_eq!(submitted, exists);
            assert_eq!(
                (goal.objective.as_str(), goal.delegation),
                ("ship it", None)
            );

            let (submitted, goal) = goal_of(run("/team 3 lanes | fix the login ", exists));
            assert_eq!(submitted, exists);
            assert_eq!(goal.objective, "fix the login");
            assert_eq!(
                goal.delegation,
                Some(Delegation::Team {
                    value: "3 lanes".into()
                })
            );
        }
    }

    #[test]
    fn usage_errors_keep_the_legacy_wording() {
        for (text, exists, notice) in [
            ("/goal", true, "usage: /goal <text>"),
            (
                "/team bad",
                true,
                "usage: /team <delegation value> | <goal>",
            ),
            (
                "/team | goal",
                true,
                "usage: /team <delegation value> | <goal>",
            ),
            ("/compaction now", true, "usage: /compaction"),
            ("/compaction", false, "Select or start a thread first"),
            ("/btw", true, "usage: /btw <question>"),
            ("/btw why?", false, "Select or start a thread first"),
            ("/run", true, "usage: /run <text>"),
            ("/continue now", true, "usage: /continue"),
            ("/continue", false, "Select or start a thread first"),
            ("/foo", true, "unknown command /foo — type /help"),
        ] {
            assert_eq!(run(text, exists), Outcome::Usage(notice.into()), "{text}");
        }
    }

    #[test]
    fn compaction_continue_help_new_and_chat() {
        assert_eq!(
            run("/compaction", true),
            Outcome::Command(Box::new(Command::Conversation(
                ConversationCommand::CompactChat(ThreadTarget {
                    thread: ThreadId::new("t").unwrap()
                })
            )))
        );
        assert_eq!(
            run("/continue", true),
            Outcome::Command(Box::new(Command::Conversation(
                ConversationCommand::ContinueChat(ContinueChat {
                    project: ProjectId::new("p").unwrap(),
                    thread: ThreadId::new("t").unwrap(),
                    composer_role: ComposerRole::Orchestrator,
                    model_preference: None,
                })
            )))
        );
        let Outcome::Help(help) = run("/help", true) else {
            panic!("help")
        };
        assert!(help.starts_with("/team <delegation value> | <goal> — "));
        assert!(help.contains("/continue — Resume the current conversation after a stop or error"));
        assert!(help.contains("/run <text> — ") && help.contains("/btw <question> — "));
        assert_eq!(run("/new", false), Outcome::NewConversation);
        assert_eq!(run("/btw  why? ", true), Outcome::Btw("why?".into()));
        assert_eq!(run("/run  fix it ", false), Outcome::Run("fix it".into()));
        assert_eq!(run("hello /goal", true), Outcome::Chat);
        assert_eq!(run("", true), Outcome::Chat);
    }
}
