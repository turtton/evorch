//! Opt-in conversation-only desktop. This module does not open a database,
//! construct a runtime, read a sidebar, or own an agent lifecycle.
pub mod cache;
pub mod connection;
pub mod features;
use features::{FeatureFrame, FeatureView, Navigation, Selection};

use base64::{Engine, engine::general_purpose::STANDARD};
use connection::Action;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_client::ClientConfig;
use workbench_protocol::namespaces::{
    ApprovalsCommand, ConversationCommand, ExecutionCommand, QuestionsCommand,
};
use workbench_protocol::{
    commands::*,
    handshake::{BuildInfo, Capability},
    ids::*,
    views::*,
};

#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub revision: u64,
}

#[derive(Clone)]
pub struct ImageDraft {
    pub id: uuid::Uuid,
    /// Queued, in flight, or OutcomeUnknown: never reallocate until resolved.
    pub uploading: bool,
    pub mime: String,
    pub data: String,
    pub reference: Option<ImageRef>,
}

#[derive(Default)]
pub struct Shared {
    pub cache: cache::Cache,
    pub drafts: BTreeMap<String, Draft>,
    pub image_drafts: BTreeMap<String, Vec<ImageDraft>>,
    pub sending: std::collections::BTreeSet<String>,
    pub outcome_unknown: std::collections::BTreeSet<String>,
    pub connected: bool,
    pub status: String,
    pub changed: Arc<tokio::sync::Notify>,
    /// What the connected core offers; features are shown only for what it does.
    pub capabilities: Vec<Capability>,
    /// The per-area features, fed by the shared connection (`features/`).
    pub features: features::Features,
}

impl Shared {
    pub fn take_image_drafts(&mut self, thread: &str) -> Option<Vec<ImageDraft>> {
        if !self.can_discard_images(thread) {
            return None;
        }
        self.image_drafts.remove(thread)
    }
    pub fn can_discard_images(&self, thread: &str) -> bool {
        !self.sending.contains(thread)
            && !self
                .image_drafts
                .get(thread)
                .into_iter()
                .flatten()
                .any(|i| i.uploading)
    }

    pub fn can_send(&self, thread: &str) -> bool {
        !self.sending.contains(thread)
            && !self
                .image_drafts
                .get(thread)
                .into_iter()
                .flatten()
                .any(|i| i.reference.is_none() || i.uploading)
    }

    fn image_draft(&mut self, thread: &str, mime: String, data: String) -> &mut ImageDraft {
        let images = self.image_drafts.entry(thread.to_owned()).or_default();
        let position = images
            .iter()
            .position(|i| i.mime == mime && i.data == data)
            .unwrap_or_else(|| {
                images.push(ImageDraft {
                    id: uuid::Uuid::new_v4(),
                    uploading: false,
                    mime,
                    data,
                    reference: None,
                });
                images.len() - 1
            });
        &mut images[position]
    }

    /// A snapshot cut arrived (connect, reconnect or resync): replace the
    /// projection, then restart the features on it. Features see their reset
    /// first, then the events that arrived during the cut, then the `held`
    /// replies (resolved outcomes of commands whose reply was lost), so a reset
    /// never discards a resolved reply.
    pub fn begin_session(
        &mut self,
        cache: cache::Cache,
        capabilities: Vec<Capability>,
        delivered: &[workbench_protocol::events::UiEvent],
        held: Vec<features::HeldReply>,
    ) {
        self.replace_projection(cache);
        self.capabilities = capabilities;
        self.features.begin_session(delivered, held);
        self.connected = true;
        self.status = "core-client: 接続済み".into();
    }

    /// Replace a disposable cut without discarding human input or uncertainty.
    pub fn replace_projection(&mut self, cache: cache::Cache) {
        if self.cache.cursor().map(|c| c.core_epoch) != cache.cursor().map(|c| c.core_epoch) {
            for image in self.image_drafts.values_mut().flatten() {
                image.reference = None;
            }
        }
        self.cache = cache;
    }
}

pub fn client_config() -> ClientConfig {
    let mut config = ClientConfig::new(
        ClientId::new(format!("desktop-{}", uuid::Uuid::new_v4())).expect("uuid fits"),
        BuildInfo {
            name: "evorch-gui".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            revision: None,
        },
    );
    config.required_capabilities = vec![
        Capability::Commands,
        Capability::Queries,
        Capability::Conversation,
        Capability::SnapshotEvents,
        Capability::InputReceipts,
        Capability::Questions,
        Capability::Approvals,
        Capability::CommittedBodies,
    ];
    config
}

pub fn run(config_dir: PathBuf, title: String) -> Result<(), String> {
    let identity = core_instance::ConfigIdentity::new(&config_dir).map_err(|e| e.to_string())?;
    let binary = core_launcher::CoreBinary::sibling().map_err(|e| e.to_string())?;
    let launcher = core_launcher::Launcher::new(identity, binary, client_config())
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let options = crate::window::native_options(&title);
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let task = runtime.spawn(connection::serve(
                launcher,
                rx,
                shared.clone(),
                cc.egui_ctx.clone(),
            ));
            Ok(Box::new(CoreClientApp {
                shared,
                actions: tx,
                selected: None,
                project: None,
                answers: BTreeMap::new(),
                attachment_path: String::new(),
                composer_role: ComposerRole::Worker,
                team: false,
                _runtime: runtime,
                task,
            }))
        }),
    )
    .map_err(|e| e.to_string())
}

pub struct CoreClientApp {
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    selected: Option<ThreadId>,
    project: Option<ProjectId>,
    answers: BTreeMap<String, String>,
    attachment_path: String,
    composer_role: ComposerRole,
    team: bool,
    _runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for CoreClientApp {
    fn drop(&mut self) {
        // Give the actor a bounded best-effort chance to discard known unused
        // attachments. This is resource cleanup, never an agent stop command.
        self._runtime.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let _ = self.actions.send(Action::Shutdown).await;
                let _ = (&mut self.task).await;
            })
            .await;
        });
        self.task.abort();
    }
}

/// Apply what the features asked for in a frame: send their requests and make
/// the selection changes they named, as a click on the built-in selector would.
fn apply_frame(
    tx: &tokio::sync::mpsc::Sender<Action>,
    shared: &mut Shared,
    project: &mut Option<ProjectId>,
    selected: &mut Option<ThreadId>,
    frame: FeatureFrame,
) {
    for action in frame.actions {
        enqueue(tx, shared, Action::Feature(action));
    }
    for navigation in frame.navigation {
        match navigation {
            Navigation::SelectProject(id) => {
                *project = Some(id);
                *selected = None;
            }
            Navigation::SelectThread {
                project: id,
                thread,
            } => {
                *project = Some(id);
                *selected = Some(thread.clone());
                enqueue(tx, shared, Action::History(thread, false));
            }
            Navigation::NewConversation(id) => {
                *project = Some(id);
                *selected = Some(ThreadId::new(uuid::Uuid::new_v4().to_string()).unwrap());
            }
        }
    }
}

/// Register local input before making it visible to the asynchronous actor.
pub fn enqueue(tx: &tokio::sync::mpsc::Sender<Action>, shared: &mut Shared, action: Action) {
    match &action {
        Action::Command {
            draft: Some((thread, _)),
            ..
        } => {
            if !shared.can_send(thread) {
                return;
            }
            shared.sending.insert(thread.clone());
        }
        Action::Upload {
            thread, mime, data, ..
        } => {
            if shared.sending.contains(thread.as_str()) {
                return;
            }
            let image = shared.image_draft(thread.as_str(), mime.clone(), data.clone());
            if image.uploading || image.reference.is_some() {
                return;
            }
            image.uploading = true;
        }
        _ => (),
    }
    if let Err(error) = tx.try_send(action) {
        match error.into_inner() {
            Action::Feature(action) => {
                shared
                    .features
                    .refuse(&action, "client queue is full or closed");
            }
            Action::Command {
                draft: Some((thread, _)),
                ..
            } => {
                shared.sending.remove(&thread);
            }
            Action::Upload {
                thread, mime, data, ..
            } => {
                shared.image_draft(thread.as_str(), mime, data).uploading = false;
            }
            _ => (),
        }
        shared.status =
            "クライアントの送信キューが満杯または切断中です。入力は保持されています。".into();
    }
}

impl eframe::App for CoreClientApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        ui.heading("evorch — core-client（試験モード）");
        ui.horizontal(|ui| {
            ui.label(&shared.status);
            if ui.button("再接続 / 再同期").clicked() {
                enqueue(&self.actions, &mut shared, Action::Reconnect);
            }
        });
        for request in &shared.outcome_unknown {
            ui.colored_label(
                egui::Color32::YELLOW,
                format!("OutcomeUnknown: {request}（再送していません）"),
            );
        }
        let frame = {
            let selection = Selection {
                project: self.project.clone(),
                thread: self.selected.clone(),
            };
            let s = &mut *shared;
            let view = FeatureView {
                connected: s.connected,
                capabilities: &s.capabilities,
                cache: &s.cache,
                selection: &selection,
            };
            s.features.render(ui, view)
        };
        apply_frame(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            frame,
        );
        // The view names no unsupported feature itself: each area says what it
        // cannot do (`Feature::unsupported`).
        let notes = shared.features.unsupported_notes();
        ui.collapsing("未対応の操作 — Unsupported", |ui| {
            ui.label("このモードでは、画面に表示されている操作だけが使用できます。それ以外は使用できません。直接 DB を開いて補完しません。");
            for (title, note) in notes {
                ui.label(format!("{title}: {note}"));
            }
        });
        if !shared.connected {
            ui.label("コアへの接続が必要です。embedded runtime へは fallback しません。");
        }
        ui.add_enabled_ui(shared.connected, |ui| {
            // A feature may draw the selector (the workspace one, once implemented);
            // while none does, the built-in selector below is unchanged.
            let replaced = {
                let selection = Selection {
                    project: self.project.clone(),
                    thread: self.selected.clone(),
                };
                let s = &mut *shared;
                let view = FeatureView {
                    connected: s.connected,
                    capabilities: &s.capabilities,
                    cache: &s.cache,
                    selection: &selection,
                };
                s.features.render_selector(ui, view)
            };
            // What the features asked for is applied whether or not one of them drew
            // the selector.
            let builtin_selector = !replaced.replaced;
            apply_frame(&self.actions, &mut shared, &mut self.project, &mut self.selected, replaced.frame);
            if builtin_selector {
            ui.horizontal(|ui| {
                let projects: Vec<_> = shared.cache.projects.values().cloned().collect();
                if self.project.is_none() { self.project = projects.first().map(|p| p.id.clone()); }
                egui::ComboBox::from_id_salt("core-project").selected_text(self.project.as_ref().map_or("project", ProjectId::as_str)).show_ui(ui, |ui| {
                    for project in projects { if ui.selectable_value(&mut self.project, Some(project.id), project.display_name).changed() { self.selected = None; } }
                });
                if ui.button("新しい会話").clicked() && self.project.is_some() {
                    self.selected = Some(ThreadId::new(uuid::Uuid::new_v4().to_string()).unwrap());
                }
            });
            let threads: Vec<_> = shared.cache.threads.values().filter(|t| Some(&t.project) == self.project.as_ref()).cloned().collect();
            egui::ComboBox::from_id_salt("core-thread").selected_text(self.selected.as_ref().and_then(|id| shared.cache.threads.get(id.as_str())).map_or("会話を選択", |t| t.title.as_str())).show_ui(ui, |ui| {
                for thread in threads {
                    if ui.selectable_value(&mut self.selected, Some(thread.id.clone()), format!("{} ({:?})", thread.title, thread.state)).changed() {
                        enqueue(&self.actions, &mut shared, Action::History(thread.id, false));
                    }
                }
            });
            }
            let (Some(thread), Some(project)) = (self.selected.clone(), self.project.clone()) else { return };
            ui.horizontal(|ui| {
                if ui.button("履歴更新").clicked() { enqueue(&self.actions, &mut shared, Action::History(thread.clone(), false)); }
                if shared.cache.older.get(thread.as_str()).is_some_and(Option::is_some) && ui.button("以前の履歴").clicked() { enqueue(&self.actions, &mut shared, Action::History(thread.clone(), true)); }
                if ui.button("停止済み legacy 会話の引継ぎ").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::PrepareExistingThread { project: project.clone(), thread: thread.clone() }), draft: None });
                }
                if ui.button("続行").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::ContinueChat(ContinueChat { project: project.clone(), thread: thread.clone(), composer_role: self.composer_role, model_preference: None })), draft: None });
                }
                if ui.button("停止").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Execution(ExecutionCommand::StopChat(ThreadTarget { thread: thread.clone() })), draft: None });
                }
            });
            egui::ScrollArea::vertical().id_salt("committed-history").max_height(ui.available_height() * 0.45).show(ui, |ui| {
                for message in shared.cache.history.get(thread.as_str()).cloned().unwrap_or_default() {
                    ui.label(format!("{:?}", message.role));
                    match message.content {
                        MessageContent::Inline { text } => { ui.label(text); }
                        MessageContent::Body(body) => {
                            if let Some(text) = shared.cache.bodies.get(body.body_id.as_str()) { ui.label(text); }
                            else if ui.button(format!("確定本文を読む（{} bytes）", body.total_bytes.get())).clicked() {
                                enqueue(&self.actions, &mut shared, Action::Body(body.body_id));
                            }
                        }
                    }
                    ui.separator();
                }
            });
            let runs: Vec<_> = shared.cache.runs.values().filter(|r| r.thread == thread).cloned().collect();
            ui.collapsing("run", |ui| {
                for run in runs {
                    ui.horizontal(|ui| {
                        ui.label(format!("{} {:?}", run.id, run.phase));
                        if ui.button("この run を停止").clicked() {
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::Execution(ExecutionCommand::StopRun(StopRun { thread: thread.clone(), run: run.id })), draft: None });
                        }
                    });
                }
            });
            let questions: Vec<_> = shared.cache.questions.values().filter(|q| q.answer.is_none() && (q.requester.thread == thread || matches!(&q.target, QuestionTarget::Lead { lead_thread, .. } if lead_thread == &thread))).cloned().collect();
            for question in questions {
                ui.label(&question.title);
                let answer = self.answers.entry(question.id.to_string()).or_default();
                ui.text_edit_singleline(answer);
                for option in &question.options { if ui.button(option).clicked() { *answer = option.clone(); } }
                if ui.button("質問に回答").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Questions(QuestionsCommand::AnswerUserQuestion(AnswerUserQuestion { thread: thread.clone(), question_id: question.id, answer: answer.clone(), question_revision: question.routing_revision })), draft: None });
                }
            }
            let approvals: Vec<_> = shared.cache.approvals.values().filter(|a| a.thread == thread && a.state == ApprovalState::Pending).cloned().collect();
            for approval in approvals {
                ui.label(format!("承認: {} — {}", approval.tool_name, approval.summary));
                if approval.summary_truncated { ui.colored_label(egui::Color32::YELLOW, "入力概要は省略されています。完全な入力を確認できない場合は承認しないでください。"); }
                if let Some(input) = shared.cache.approval_inputs.get(approval.id.as_str()) { ui.label(input); }
                else if ui.button("完全な承認入力を確認").clicked() { enqueue(&self.actions, &mut shared, Action::ApprovalInput(approval.clone())); }
                ui.horizontal(|ui| {
                    for (label, decision) in [("承認", ApprovalDecision::Approve), ("拒否", ApprovalDecision::Deny)] {
                        if ui.add_enabled(decision == ApprovalDecision::Deny || shared.cache.approval_inputs.contains_key(approval.id.as_str()), egui::Button::new(label)).clicked() {
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::Approvals(ApprovalsCommand::DecideToolApproval(DecideToolApproval { approval_id: approval.id.clone(), core_epoch: approval.core_epoch, approval_revision: approval.revision, decision })), draft: None });
                        }
                    }
                });
            }
            let receipts: Vec<_> = shared.cache.receipts.values().filter(|r| r.thread == thread).cloned().collect();
            for receipt in receipts {
                ui.horizontal(|ui| {
                    ui.label(format!("入力 {}: {:?}", receipt.submission_id, receipt.state));
                    if receipt.state == InputReceiptState::Accepted && let Some(run) = receipt.run
                        && ui.button("follow-up を取消").clicked() {
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::WithdrawFollowUp(WithdrawFollowUp { thread: thread.clone(), run, submission_id: receipt.submission_id })), draft: None });
                        }
                });
            }
            if ui.button("follow-up を次の turn で配信").clicked() {
                enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::DeliverFollowUpsNextTurn(ThreadTarget { thread: thread.clone() })), draft: None });
            }
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.composer_role, ComposerRole::Worker, "Worker");
                ui.selectable_value(&mut self.composer_role, ComposerRole::Orchestrator, "Orchestrator");
                ui.checkbox(&mut self.team, "チームへの objective（画像は非対応）");
            });
            let draft = shared.drafts.entry(thread.to_string()).or_default();
            if ui.text_edit_multiline(&mut draft.text).changed() { draft.revision += 1; }
            ui.horizontal(|ui| {
                ui.label("画像（GUI で読み取り、bytes のみ送信）");
                ui.text_edit_singleline(&mut self.attachment_path);
                if ui.add_enabled(!shared.sending.contains(thread.as_str()), egui::Button::new("添付")).clicked() {
                    match read_image(&self.attachment_path) {
                        Ok((mime, data)) => enqueue(&self.actions, &mut shared, Action::Upload { project: project.clone(), thread: thread.clone(), mime, data }),
                        Err(error) => shared.status = error,
                    }
                }
            });
            ui.label(format!("添付済み: {}", shared.image_drafts.get(thread.as_str()).map_or(0, |images| images.iter().filter(|i| i.reference.is_some()).count())));
            if ui.add_enabled(shared.can_discard_images(thread.as_str()), egui::Button::new("画像を破棄")).clicked() {
                for image in shared.take_image_drafts(thread.as_str()).into_iter().flatten().filter_map(|image| image.reference) {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::DiscardAttachment { attachment_id: image.attachment_id }), draft: None });
                }
            }
            let upload_pending = shared.image_drafts.get(thread.as_str()).is_some_and(|images| images.iter().any(|i| i.reference.is_none()));
            if upload_pending && ui.button("未送信の画像を再検証").clicked() {
                for image in shared.image_drafts.get(thread.as_str()).cloned().unwrap_or_default().into_iter().filter(|i| i.reference.is_none() && !i.uploading) {
                    enqueue(&self.actions, &mut shared, Action::Upload { project: project.clone(), thread: thread.clone(), mime: image.mime, data: image.data });
                }
            }
            if ui.add_enabled(shared.can_send(thread.as_str()), egui::Button::new("送信 / follow-up")).clicked() {
                let draft = shared.drafts.get(thread.as_str()).unwrap();
                let revision = draft.revision;
                let chat = SendChat { project, thread: thread.clone(), submission_id: SubmissionId::new(uuid::Uuid::new_v4().to_string()).unwrap(), composer_role: self.composer_role, text: draft.text.clone(), skills: vec![], images: shared.image_drafts.get(thread.as_str()).into_iter().flatten().filter_map(|i| i.reference.clone()).collect(), model_preference: None, fork_seed: None };
                if self.team && !chat.images.is_empty() { shared.status = "チーム objective への画像添付は Unsupported".into(); return; }
                let command = if self.team {
                    let goal = SubmitGoal { project: chat.project.clone(), thread: thread.clone(), objective: chat.text.clone(), references: vec![], constraints: vec![], delegation: Some(Delegation::Team { value: chat.text }) };
                    if shared.cache.threads.contains_key(thread.as_str()) { Command::Execution(ExecutionCommand::SubmitGoal(goal)) } else { Command::Conversation(ConversationCommand::CreateGoal(goal)) }
                } else if shared.cache.threads.contains_key(thread.as_str()) { Command::Conversation(ConversationCommand::SendChat(chat)) } else { Command::Conversation(ConversationCommand::CreateExecution(chat)) };
                enqueue(&self.actions, &mut shared, Action::Command { command, draft: Some((thread.to_string(), revision)) });
            }
        });
    }
}

fn read_image(path: &str) -> Result<(String, String), String> {
    use std::io::Read;
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    file.take(384 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() > 384 * 1024 {
        return Err("画像は 384 KiB 以下にしてください".into());
    }
    let format = image::guess_format(&bytes).map_err(|e| e.to_string())?;
    Ok((format.to_mime_type().into(), STANDARD.encode(bytes)))
}

#[cfg(test)]
mod upload_regressions {
    use super::*;

    fn upload() -> Action {
        Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            mime: "image/png".into(),
            data: "local bytes".into(),
        }
    }
    fn send() -> Action {
        Action::Command {
            command: Command::Execution(ExecutionCommand::StopChat(ThreadTarget {
                thread: ThreadId::new("t").unwrap(),
            })),
            draft: Some(("t".into(), 0)),
        }
    }
    #[test]
    fn attach_then_send_before_actor_runs_keeps_image_and_blocks_send() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        assert!(
            state.image_drafts.get("t").is_some_and(|i| i.len() == 1),
            "enqueue must register the image synchronously"
        );
        enqueue(&tx, &mut state, send());
        assert!(!state.sending.contains("t"));
        assert!(matches!(rx.try_recv(), Ok(Action::Upload { .. })));
        assert!(rx.try_recv().is_err(), "send must not overtake upload");
    }
    #[test]
    fn full_queue_keeps_local_image_for_explicit_retry() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(Action::Reconnect).ok().unwrap();
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        assert_eq!(state.image_drafts["t"][0].data, "local bytes");
        assert!(state.image_drafts["t"][0].reference.is_none());
    }
    #[tokio::test]
    async fn discard_while_upload_reply_is_held_keeps_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        let Action::Upload { mime, data, .. } = rx.recv().await.unwrap() else {
            panic!()
        };
        // Model the old actor registration before its network await. The reply
        // cannot arrive until the test explicitly releases this oneshot.
        state.image_drafts.entry("t".into()).or_insert_with(|| {
            vec![ImageDraft {
                id: uuid::Uuid::new_v4(),
                uploading: true,
                mime,
                data,
                reference: None,
            }]
        });
        let (release, held) = tokio::sync::oneshot::channel();
        let reply = tokio::spawn(async move {
            held.await.unwrap();
        });
        assert!(
            state.take_image_drafts("t").is_none(),
            "discard must be refused until upload has a known outcome"
        );
        release.send(()).unwrap();
        reply.await.unwrap();
        assert_eq!(state.image_drafts["t"].len(), 1);
    }
}

#[cfg(test)]
mod navigation_tests {
    use super::*;

    fn ids() -> (ProjectId, ThreadId) {
        (ProjectId::new("p").unwrap(), ThreadId::new("t").unwrap())
    }

    #[test]
    fn navigation_changes_the_selection_like_the_built_in_selector() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut shared = Shared::default();
        let (id, thread) = ids();
        let (mut project, mut selected) = (None, None);
        let frame = |navigation| FeatureFrame {
            actions: vec![],
            navigation: vec![navigation],
        };

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::SelectThread {
                project: id.clone(),
                thread: thread.clone(),
            }),
        );
        assert_eq!(
            (project.as_ref(), selected.as_ref()),
            (Some(&id), Some(&thread))
        );
        // Selecting a thread loads its history, as the built-in selector does.
        assert!(matches!(rx.try_recv(), Ok(Action::History(t, false)) if t == thread));

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::SelectProject(id.clone())),
        );
        assert_eq!((project.as_ref(), selected.as_ref()), (Some(&id), None));

        apply_frame(
            &tx,
            &mut shared,
            &mut project,
            &mut selected,
            frame(Navigation::NewConversation(id.clone())),
        );
        assert_eq!(project.as_ref(), Some(&id));
        assert!(selected.is_some() && selected != Some(thread));
        assert!(
            rx.try_recv().is_err(),
            "only selecting a thread sends anything"
        );
    }
}
