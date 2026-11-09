//! Opt-in conversation-only desktop. This module does not open a database,
//! construct a runtime, read a sidebar, or own an agent lifecycle.
pub mod cache;
pub mod connection;

use base64::{Engine, engine::general_purpose::STANDARD};
use connection::Action;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_client::ClientConfig;
use workbench_protocol::{
    commands::*,
    conversation::ConversationCommand,
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
}

impl Shared {
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
        // Aborting a transport actor is not an agent stop. The independent core
        // keeps its runtime and any already accepted inputs.
        self.task.abort();
    }
}

fn enqueue(tx: &tokio::sync::mpsc::Sender<Action>, shared: &mut Shared, action: Action) {
    if let Action::Command {
        draft: Some((thread, _)),
        ..
    } = &action
        && !shared.sending.insert(thread.clone())
    {
        return;
    }
    let draft_thread = match &action {
        Action::Command {
            draft: Some((thread, _)),
            ..
        } => Some(thread.clone()),
        _ => None,
    };
    if tx.try_send(action).is_err() {
        if let Some(thread) = draft_thread {
            shared.sending.remove(&thread);
        }
        shared.status = "クライアントの送信キューが満杯です。入力は保持されています。".into();
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
        ui.collapsing("未移行の機能 — Unsupported", |ui| {
            ui.label("設定変更・モデル設定・project 追加・lane 管理・PR 管理・ファイル/terminal/browser・fork/rewind・削除/アーカイブ等はこのモードでは使用できません。直接 DB を開いて補完しません。");
        });
        if !shared.connected {
            ui.label("コアへの接続が必要です。embedded runtime へは fallback しません。");
        }
        ui.add_enabled_ui(shared.connected, |ui| {
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
            let (Some(thread), Some(project)) = (self.selected.clone(), self.project.clone()) else { return };
            ui.horizontal(|ui| {
                if ui.button("履歴更新").clicked() { enqueue(&self.actions, &mut shared, Action::History(thread.clone(), false)); }
                if shared.cache.older.get(thread.as_str()).is_some_and(Option::is_some) && ui.button("以前の履歴").clicked() { enqueue(&self.actions, &mut shared, Action::History(thread.clone(), true)); }
                if ui.button("停止済み legacy 会話の引継ぎ").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::PrepareExistingThread { project: project.clone(), thread: thread.clone() }), draft: None });
                }
                if ui.button("続行").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::ContinueChat(ContinueChat { project: project.clone(), thread: thread.clone(), composer_role: self.composer_role, model_preference: None }), draft: None });
                }
                if ui.button("停止").clicked() {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::StopChat(ThreadTarget { thread: thread.clone() }), draft: None });
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
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::StopRun(StopRun { thread: thread.clone(), run: run.id }), draft: None });
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
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::AnswerUserQuestion(AnswerUserQuestion { thread: thread.clone(), question_id: question.id, answer: answer.clone(), question_revision: question.routing_revision }), draft: None });
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
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::DecideToolApproval(DecideToolApproval { approval_id: approval.id.clone(), core_epoch: approval.core_epoch, approval_revision: approval.revision, decision }), draft: None });
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
                            enqueue(&self.actions, &mut shared, Action::Command { command: Command::WithdrawFollowUp(WithdrawFollowUp { thread: thread.clone(), run, submission_id: receipt.submission_id }), draft: None });
                        }
                });
            }
            if ui.button("follow-up を次の turn で配信").clicked() {
                enqueue(&self.actions, &mut shared, Action::Command { command: Command::DeliverFollowUpsNextTurn(ThreadTarget { thread: thread.clone() }), draft: None });
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
            if ui.add_enabled(!shared.sending.contains(thread.as_str()), egui::Button::new("画像を破棄")).clicked() {
                for image in shared.image_drafts.remove(thread.as_str()).into_iter().flatten().filter_map(|image| image.reference) {
                    enqueue(&self.actions, &mut shared, Action::Command { command: Command::Conversation(ConversationCommand::DiscardAttachment { attachment_id: image.attachment_id }), draft: None });
                }
            }
            let upload_pending = shared.image_drafts.get(thread.as_str()).is_some_and(|images| images.iter().any(|i| i.reference.is_none()));
            if upload_pending && ui.button("未送信の画像を再検証").clicked() {
                for image in shared.image_drafts.get(thread.as_str()).cloned().unwrap_or_default().into_iter().filter(|i| i.reference.is_none()) {
                    enqueue(&self.actions, &mut shared, Action::Upload { project: project.clone(), thread: thread.clone(), mime: image.mime, data: image.data });
                }
            }
            if ui.add_enabled(!shared.sending.contains(thread.as_str()) && !upload_pending, egui::Button::new("送信 / follow-up")).clicked() {
                let draft = shared.drafts.get(thread.as_str()).unwrap();
                let revision = draft.revision;
                let chat = SendChat { project, thread: thread.clone(), submission_id: SubmissionId::new(uuid::Uuid::new_v4().to_string()).unwrap(), composer_role: self.composer_role, text: draft.text.clone(), skills: vec![], images: shared.image_drafts.get(thread.as_str()).into_iter().flatten().filter_map(|i| i.reference.clone()).collect(), model_preference: None, fork_seed: None };
                if self.team && !chat.images.is_empty() { shared.status = "チーム objective への画像添付は Unsupported".into(); return; }
                let command = if self.team {
                    let goal = SubmitGoal { project: chat.project.clone(), thread: thread.clone(), objective: chat.text.clone(), references: vec![], constraints: vec![], delegation: Some(Delegation::Team { value: chat.text }) };
                    if shared.cache.threads.contains_key(thread.as_str()) { Command::SubmitGoal(goal) } else { Command::Conversation(ConversationCommand::CreateGoal(goal)) }
                } else if shared.cache.threads.contains_key(thread.as_str()) { Command::SendChat(chat) } else { Command::Conversation(ConversationCommand::CreateExecution(chat)) };
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
