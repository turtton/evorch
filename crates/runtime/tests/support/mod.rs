#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use agents::Role;
use async_trait::async_trait;
use event_bus::{Event, EventReceiver};
use providers::{
    ChatResponse, ContentBlock, FinishReason, Message, Role as MessageRole, ToolSpec, Usage,
};
use runtime::{
    AgentInvocationContext, AgentModel, ExecutionPolicy, IsolatedMounts, RuntimeError,
    SandboxFactory,
};
use sandbox::{DirectSandbox, Sandbox, SandboxError};
use tempfile::TempDir;
use tokio::sync::{Mutex, Notify};
use tokio::time::{Duration, timeout};

pub struct ScriptedModel {
    script: Mutex<VecDeque<Result<ChatResponse, RuntimeError>>>,
    keyed: Mutex<HashMap<String, VecDeque<Result<ChatResponse, RuntimeError>>>>,
    observed: Mutex<Vec<Vec<Message>>>,
    gate: Option<Arc<Notify>>,
    selected_model: Option<String>,
}

struct RecordingSandboxFactory {
    mounts: Arc<std::sync::Mutex<Vec<IsolatedMounts>>>,
}

impl SandboxFactory for RecordingSandboxFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        self.mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(mounts.clone());
        Ok(Arc::new(DirectSandbox::new_unchecked()))
    }
}

impl ScriptedModel {
    pub fn new(script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            keyed: Mutex::new(HashMap::new()),
            observed: Mutex::new(Vec::new()),
            gate: None,
            selected_model: None,
        }
    }

    pub fn gated(
        script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>,
        gate: Arc<Notify>,
    ) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
            keyed: Mutex::new(HashMap::new()),
            observed: Mutex::new(Vec::new()),
            gate: Some(gate),
            selected_model: None,
        }
    }

    pub fn with_selected_model(mut self, model: &str) -> Self {
        self.selected_model = Some(model.to_owned());
        self
    }

    pub async fn add_keyed(
        &self,
        marker: &str,
        script: impl IntoIterator<Item = Result<ChatResponse, RuntimeError>>,
    ) {
        self.keyed
            .lock()
            .await
            .insert(marker.to_string(), script.into_iter().collect());
    }

    pub async fn observed(&self) -> Vec<Vec<Message>> {
        self.observed.lock().await.clone()
    }
}

#[async_trait]
impl AgentModel for ScriptedModel {
    async fn complete(
        &self,
        _invocation: &AgentInvocationContext,
        _role: Role,
        messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<ChatResponse, RuntimeError> {
        self.observed.lock().await.push(messages.to_vec());
        if let Some(gate) = &self.gate {
            gate.notified().await;
        }

        let marker = messages
            .iter()
            .find(|message| message.role == MessageRole::User)
            .and_then(|message| {
                message.content.iter().find_map(|block| match block {
                    ContentBlock::Text { text } => Some(text.as_str()),
                    ContentBlock::Reasoning { .. }
                    | ContentBlock::ToolUse { .. }
                    | ContentBlock::ToolResult { .. } => None,
                })
            });
        if let Some(marker) = marker {
            let mut keyed = self.keyed.lock().await;
            if let Some(script) = keyed.get_mut(marker) {
                return script.pop_front().unwrap_or_else(|| {
                    Err(RuntimeError::Model {
                        reason: format!("script exhausted for {marker}"),
                    })
                });
            }
        }

        self.script.lock().await.pop_front().unwrap_or_else(|| {
            Err(RuntimeError::Model {
                reason: "script exhausted".to_string(),
            })
        })
    }

    fn selected_model(&self, role: Role) -> String {
        self.selected_model
            .clone()
            .unwrap_or_else(|| format!("scripted-{}", role.name().to_lowercase()))
    }
}

pub fn text_response(text: &str, finish_reason: FinishReason) -> ChatResponse {
    response(
        vec![ContentBlock::Text {
            text: text.to_string(),
        }],
        finish_reason,
    )
}

pub fn tool_response(id: &str, name: &str, input: serde_json::Value) -> ChatResponse {
    response(
        vec![ContentBlock::ToolUse {
            id: id.to_string(),
            name: name.to_string(),
            input,
        }],
        FinishReason::ToolUse,
    )
}

pub fn tool_responses(
    uses: impl IntoIterator<Item = (&'static str, &'static str, serde_json::Value)>,
) -> ChatResponse {
    response(
        uses.into_iter()
            .map(|(id, name, input)| ContentBlock::ToolUse {
                id: id.to_string(),
                name: name.to_string(),
                input,
            })
            .collect(),
        FinishReason::ToolUse,
    )
}

fn response(content: Vec<ContentBlock>, finish_reason: FinishReason) -> ChatResponse {
    ChatResponse {
        message: Message {
            role: MessageRole::Assistant,
            content,
        },
        usage: Usage::default(),
        finish_reason,
    }
}

pub async fn collect_events(receiver: &mut EventReceiver, count: usize) -> Vec<Event> {
    let mut events = Vec::with_capacity(count);
    while events.len() < count {
        let event = timeout(Duration::from_secs(2), receiver.recv())
            .await
            .expect("event timeout")
            .expect("event receiver remains open");
        events.push(event);
    }
    events
}

pub fn recording_factory() -> (
    Arc<dyn SandboxFactory>,
    Arc<std::sync::Mutex<Vec<IsolatedMounts>>>,
) {
    let mounts = Arc::new(std::sync::Mutex::new(Vec::new()));
    (
        Arc::new(RecordingSandboxFactory {
            mounts: Arc::clone(&mounts),
        }),
        mounts,
    )
}

pub fn init_git_repo() -> (TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("一時ディレクトリを作成できる");
    let repo = temp.path().join("repo");
    fs::create_dir(&repo).expect("リポジトリ用ディレクトリを作成できる");
    assert!(git(&repo, &["init"]).status.success());
    assert!(
        git(&repo, &["config", "user.name", "Evorch Test"])
            .status
            .success()
    );
    assert!(
        git(&repo, &["config", "user.email", "evorch@example.invalid"])
            .status
            .success()
    );
    fs::write(repo.join("README.md"), "# test\n").expect("初期ファイルを書き込める");
    assert!(git(&repo, &["add", "README.md"]).status.success());
    assert!(git(&repo, &["commit", "-m", "initial"]).status.success());
    (temp, repo)
}

pub fn git(repo: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .expect("git を実行できる")
}

/// Loads the long coding-session fixture used by compaction fidelity (AC3)
/// and continuation (AC9) tests of issue #63.
///
/// Provenance: the fixture shape is derived from opencode (sst/opencode)
/// compaction summary+tail layout, senpi/pi-mono `CompactionEntry` +
/// `firstKeptEntryId` cut-point rules (a tail cut must not sever an open tool
/// pair), and the omo compress section model (Goal / Tasks / Decisions /
/// Files / Verification / Open items) mapped onto assistant text and user
/// tool results.
pub fn load_compaction_fixture() -> Vec<Message> {
    let raw = include_str!("../fixtures/compaction_long_session.json");
    serde_json::from_str(raw)
        .expect("compaction long-session fixture must parse as Vec<providers::Message>")
}
