//! The recorded thread a replay starts from, read from the user's database (ADR 0036 §5).

use std::path::{Path, PathBuf};

use runtime::Role;
use serde::{Deserialize, Serialize};
use storage::thread_start::{ThreadStartCapture, ThreadStartSnapshot};
use storage::{Database, StorageError};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(
        "thread {0} has no recorded start state; only threads started after start recording can be replayed"
    )]
    NoStartState(String),
    #[error("thread {thread} cannot be replayed: its start state is {reason}")]
    NotReplayable {
        thread: String,
        reason: &'static str,
    },
    #[error("thread {0} used a thread goal, which replays do not support yet")]
    UsesThreadGoal(String),
    #[error("thread {thread} has no saved conversation for its first run {run}")]
    MissingRoot { thread: String, run: String },
    #[error("run {run} is not a chat root of thread {thread} (named {name:?})")]
    NotAChatRoot {
        thread: String,
        run: String,
        name: String,
    },
    #[error("thread {0} has no recorded initial prompt")]
    NoInitialPrompt(String),
    #[error("project root {root} does not end with the recorded repository prefix {prefix:?}")]
    PrefixMismatch { root: String, prefix: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UserTurnKind {
    InitialPrompt,
    Message,
    Answer,
}

/// One thing the original user said, in thread order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserTurn {
    pub kind: UserTurnKind,
    pub text: String,
    /// The question an answer replied to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    /// The assistant text the user referenced alongside a message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
}

#[derive(Debug, Clone)]
pub struct OriginalThread {
    pub thread_id: String,
    pub role: Role,
    pub root_run_id: String,
    pub project_root: PathBuf,
    /// The repository top level of `project_root`.
    pub source_toplevel: PathBuf,
    pub captured_at_ns: i64,
    pub snapshot: ThreadStartSnapshot,
    pub turns: Vec<UserTurn>,
}

impl OriginalThread {
    pub fn initial_prompt(&self) -> &str {
        &self.turns[0].text
    }

    /// Read everything a replay of `thread_id` needs.
    ///
    /// # Errors
    /// Refuses threads without a replayable start state, threads that used a
    /// thread goal, and threads whose conversation cannot be reconstructed.
    pub fn load(database: &Database, thread_id: &str) -> Result<Self, SourceError> {
        let thread = || thread_id.to_owned();
        let start = database
            .thread_start_state(thread_id)?
            .ok_or_else(|| SourceError::NoStartState(thread()))?;
        let snapshot = match start.capture {
            ThreadStartCapture::Replayable(snapshot) => snapshot,
            ThreadStartCapture::NotReplayable(reason) => {
                return Err(SourceError::NotReplayable {
                    thread: thread(),
                    reason: reason.as_str(),
                });
            }
        };
        if database.thread_goal_recorded(thread_id)? {
            return Err(SourceError::UsesThreadGoal(thread()));
        }
        let root =
            database
                .run_context(&start.root_run_id)?
                .ok_or_else(|| SourceError::MissingRoot {
                    thread: thread(),
                    run: start.root_run_id.clone(),
                })?;
        let role = chat_role(&root.name, thread_id)
            .filter(|_| root.parent_run_id.is_none())
            .ok_or_else(|| SourceError::NotAChatRoot {
                thread: thread(),
                run: root.run_id.clone(),
                name: root.name.clone(),
            })?;

        let mut lineage = vec![root.run_id.clone()];
        if let Some(latest) = database.latest_terminal_run_context(&root.name)? {
            lineage.extend(database.review_lineage_ancestors(&latest.run_id)?);
            lineage.push(latest.run_id);
        }
        lineage.sort();
        lineage.dedup();
        let turns: Vec<UserTurn> = database
            .review_evidence_for_lineage(&lineage)?
            .into_iter()
            .filter_map(|row| {
                let kind = match row.kind.as_str() {
                    "initial_prompt" => UserTurnKind::InitialPrompt,
                    "host_chat" => UserTurnKind::Message,
                    "user_answer" => UserTurnKind::Answer,
                    _ => return None,
                };
                Some(UserTurn {
                    kind,
                    text: row.text,
                    question: row.in_reply_to_title,
                    reference: row.reference_context,
                })
            })
            .collect();
        if turns.first().map(|turn| turn.kind) != Some(UserTurnKind::InitialPrompt) {
            return Err(SourceError::NoInitialPrompt(thread()));
        }

        let project_root = PathBuf::from(&start.project_root);
        let source_toplevel = strip_prefix(&project_root, &snapshot.repo_prefix)?;
        Ok(Self {
            thread_id: thread(),
            role,
            root_run_id: root.run_id,
            project_root,
            source_toplevel,
            captured_at_ns: start.captured_at_ns,
            snapshot,
            turns,
        })
    }
}

fn chat_role(name: &str, thread_id: &str) -> Option<Role> {
    let (role, thread) = name.strip_prefix("chat:")?.split_once(':')?;
    (thread == thread_id).then(|| Role::from_name(role).ok())?
}

fn strip_prefix(project_root: &Path, prefix: &str) -> Result<PathBuf, SourceError> {
    let prefix = Path::new(prefix.trim_end_matches('/'));
    let depth = prefix.components().count();
    project_root
        .ends_with(prefix)
        .then(|| project_root.ancestors().nth(depth))
        .flatten()
        .map(Path::to_path_buf)
        .ok_or_else(|| SourceError::PrefixMismatch {
            root: project_root.display().to_string(),
            prefix: prefix.display().to_string(),
        })
}

#[cfg(test)]
pub(crate) mod tests;
