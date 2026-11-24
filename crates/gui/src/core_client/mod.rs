//! Opt-in conversation-only desktop. This module does not open a database,
//! construct a runtime, read a sidebar, or own an agent lifecycle.
pub mod attachments;
pub mod auto_reconnect;
pub mod cache;
pub mod restart;
use crate::attachments::AttachmentFile;
pub use attachments::ClientAttachmentDraft;
pub mod connection;
pub mod conversation;
pub mod features;
pub mod notifications;
pub mod shell;
use features::{FeatureFrame, FeatureView, Navigation, Selection};

use connection::Action;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};
use workbench_client::ClientConfig;
use workbench_protocol::namespaces::ConversationCommand;
use workbench_protocol::{
    commands::*,
    handshake::{BuildInfo, Capability},
    ids::*,
};

#[derive(Default)]
pub struct Draft {
    pub text: String,
    pub revision: u64,
}

#[derive(Default)]
pub struct Shared {
    pub cache: cache::Cache,
    pub drafts: BTreeMap<String, Draft>,
    pub attachment_drafts: BTreeMap<String, Vec<ClientAttachmentDraft>>,
    pub sending: std::collections::BTreeSet<String>,
    pub loading_attachments: BTreeMap<String, usize>,
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
    pub fn take_attachment_drafts(&mut self, thread: &str) -> Option<Vec<ClientAttachmentDraft>> {
        if !self.can_discard_attachments(thread) {
            return None;
        }
        self.attachment_drafts.remove(thread)
    }
    pub fn can_discard_attachments(&self, thread: &str) -> bool {
        can_discard_attachments(
            &self.sending,
            &self.loading_attachments,
            &self.attachment_drafts,
            thread,
        )
    }

    pub fn can_send(&self, thread: &str) -> bool {
        can_send(
            &self.sending,
            &self.loading_attachments,
            &self.attachment_drafts,
            thread,
        )
    }

    /// A control-session transition, not a render-time observation. Call once so
    /// a collapsed/minimized feature cannot retain core-derived bodies or textures.
    fn disconnect(&mut self) {
        if self.connected {
            self.connected = false;
            self.features.disconnected();
        }
    }

    fn attachment_draft(
        &mut self,
        project: ProjectId,
        thread: &str,
        file: AttachmentFile,
    ) -> &mut ClientAttachmentDraft {
        let drafts = self.attachment_drafts.entry(thread.to_owned()).or_default();
        let position = drafts
            .iter()
            .position(|draft| draft.id == file.draft_id)
            .unwrap_or_else(|| {
                drafts.push(ClientAttachmentDraft::new(project, file));
                drafts.len() - 1
            });
        &mut drafts[position]
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
        if self.cache.cursor().is_some()
            && self.cache.cursor().map(|c| c.core_epoch) != cache.cursor().map(|c| c.core_epoch)
        {
            for image in self.attachment_drafts.values_mut().flatten() {
                if !image.uncertain {
                    image.reference = None;
                    image.upload_id = None;
                    image.next_offset = 0;
                    image.uploading = false;
                }
            }
        }
        self.cache = cache;
    }
}

fn can_discard_attachments(
    sending: &std::collections::BTreeSet<String>,
    loading: &BTreeMap<String, usize>,
    drafts: &BTreeMap<String, Vec<ClientAttachmentDraft>>,
    thread: &str,
) -> bool {
    !sending.contains(thread)
        && loading.get(thread).copied().unwrap_or(0) == 0
        && !drafts
            .get(thread)
            .into_iter()
            .flatten()
            .any(|i| i.uploading)
}

fn can_send(
    sending: &std::collections::BTreeSet<String>,
    loading: &BTreeMap<String, usize>,
    drafts: &BTreeMap<String, Vec<ClientAttachmentDraft>>,
    thread: &str,
) -> bool {
    !sending.contains(thread)
        && loading.get(thread).copied().unwrap_or(0) == 0
        && !drafts
            .get(thread)
            .into_iter()
            .flatten()
            .any(|i| i.reference.is_none() || i.uploading)
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
    let identity = core_instance::ConfigIdentity::new(&config_dir)
        .map_err(|e| format!("configuration directory {}: {e}", config_dir.display()))?;
    // The launcher starts only the evorch-core installed beside this executable. Without
    // one there is nothing to connect to or start, and no other runtime takes its place.
    let binary = core_launcher::CoreBinary::sibling().map_err(|e| {
        format!(
            "evorch-core is not available beside evorch-gui ({e}); \
             evorch-gui does not run its own runtime in its place"
        )
    })?;
    let launcher = core_launcher::Launcher::new(identity, binary, client_config())
        .map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    let options = crate::window::native_options(&title);
    let config_dir = config_dir.clone();
    eframe::run_native(
        &title,
        options,
        Box::new(move |cc| {
            crate::theme::style::install_preset(&cc.egui_ctx, theme_preset(&config_dir));
            let shared = Arc::new(Mutex::new(Shared::default()));
            let (tx, rx) = tokio::sync::mpsc::channel(64);
            let task = runtime.spawn(connection::serve(
                launcher,
                rx,
                shared.clone(),
                cc.egui_ctx.clone(),
            ));
            Ok(Box::new(CoreClientApp {
                screen: CoreClientScreen::new(shared, tx, None, None),
                _runtime: runtime,
                task,
            }))
        }),
    )
    .map_err(|e| e.to_string())
}

/// The theme the person chose in the workbench's UI settings, shared with the embedded one.
fn theme_preset(config_dir: &std::path::Path) -> crate::theme::style::ThemePreset {
    let path = config_dir.join("ui.toml");
    if !path.exists() {
        return crate::theme::style::ThemePreset::Graphite;
    }
    workspace_ui::load_settings(&path)
        .map_or(crate::theme::style::ThemePreset::Graphite, |settings| {
            settings.theme_preset.into()
        })
}

pub struct CoreClientApp {
    screen: CoreClientScreen,
    _runtime: tokio::runtime::Runtime,
    task: tokio::task::JoinHandle<()>,
}

/// What the window draws, apart from the runtime that serves the connection: the features, the
/// selector and the conversation controls, over the shared projection. It is the whole page,
/// so a test can draw exactly what the window draws.
pub struct CoreClientScreen {
    shared: Arc<Mutex<Shared>>,
    actions: tokio::sync::mpsc::Sender<Action>,
    selected: Option<ThreadId>,
    project: Option<ProjectId>,
    conversation: conversation::ConversationPane,
    dock: egui_dock::DockState<shell::CoreTab>,
    notifications: notifications::NotificationLog,
}

impl CoreClientScreen {
    pub fn new(
        shared: Arc<Mutex<Shared>>,
        actions: tokio::sync::mpsc::Sender<Action>,
        project: Option<ProjectId>,
        selected: Option<ThreadId>,
    ) -> Self {
        Self {
            shared,
            actions,
            selected,
            project,
            conversation: conversation::ConversationPane::default(),
            dock: shell::default_dock(),
            notifications: notifications::NotificationLog::default(),
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        let results = self.conversation.attachment_io.poll().collect::<Vec<_>>();
        for result in results {
            let Some((project, thread)) = self
                .conversation
                .attachment_imports
                .remove(&result.request_id)
            else {
                continue;
            };
            let mut shared = self
                .shared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(count) = shared.loading_attachments.get_mut(thread.as_str()) {
                *count = count.saturating_sub(1);
            }
            match result.files {
                Ok(files) => {
                    for file in files {
                        enqueue(
                            &self.actions,
                            &mut shared,
                            Action::Upload {
                                project: project.clone(),
                                thread: thread.clone(),
                                file,
                            },
                        );
                    }
                }
                Err(error) => shared.status = error,
            }
        }
        self.draw(ui);
    }
}

impl Drop for CoreClientApp {
    fn drop(&mut self) {
        let actions = self.screen.actions.clone();
        // Give the actor a bounded best-effort chance to discard known unused
        // attachments. This is resource cleanup, never an agent stop command.
        self._runtime.block_on(async {
            let _ = tokio::time::timeout(std::time::Duration::from_millis(500), async {
                let _ = actions.send(Action::Shutdown).await;
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
    for action in frame.data {
        enqueue(tx, shared, Action::Data(action));
    }
    for target in frame.observations {
        shared.features.observe_run(target);
    }
    for artifact in frame.previews {
        if shared.features.is_shown(
            features::FeatureId::Browser,
            shared.connected,
            &shared.capabilities,
        ) {
            shared.features.open_preview(artifact);
        } else {
            shared.status = "このコアはプレビューを提供していないため、開けません".into();
        }
    }
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
            project,
            thread,
            file,
            ..
        } => {
            if shared.sending.contains(thread.as_str()) {
                return;
            }
            let drafts = shared.attachment_drafts.get(thread.as_str());
            let fresh =
                drafts.is_none_or(|drafts| drafts.iter().all(|draft| draft.id != file.draft_id));
            if file.bytes.len() > workbench_protocol::conversation::ATTACHMENT_FILE_BYTES as usize
                || (fresh
                    && drafts.is_some_and(|drafts| {
                        drafts.len() >= 32
                            || drafts
                                .iter()
                                .map(|draft| draft.file.bytes.len())
                                .sum::<usize>()
                                + file.bytes.len()
                                > 64 * 1024 * 1024
                    }))
            {
                shared.status = "添付の上限は 8 MiB/件、32件・64 MiB/thread です".into();
                return;
            }
            let image = shared.attachment_draft(project.clone(), thread.as_str(), file.clone());
            if image.uploading || image.reference.is_some() {
                return;
            }
            image.uploading = true;
        }
        _ => (),
    }
    if let Err(error) = tx.try_send(action) {
        match error.into_inner() {
            Action::Data(action) => {
                if shared.features.data.current(&action) {
                    shared.features.data_notice(
                        action.feature,
                        action.tag,
                        features::data::DataNotice::Failed(features::FeatureError::new(
                            "client queue is full or closed",
                        )),
                    );
                }
            }
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
                project,
                thread,
                file,
                ..
            } => {
                shared
                    .attachment_draft(project, thread.as_str(), file)
                    .uploading = false;
            }
            _ => (),
        }
        shared.status =
            "クライアントの送信キューが満杯または切断中です。入力は保持されています。".into();
    }
}

impl eframe::App for CoreClientApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.screen.logic(ctx);
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.screen.ui(ui);
    }
}

impl CoreClientScreen {
    /// eframe calls logic without any UI pass while the native window is minimized.
    /// Keep the two maintenance paths disjoint, including a minimized root whose
    /// visible child viewport causes eframe to call both logic and ui.
    fn logic(&mut self, ctx: &egui::Context) {
        if !ctx.input(|input| input.viewport().minimized.unwrap_or(false)) {
            return;
        }
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
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
        let frame = s.features.maintain_minimized(view);
        // The log diffs the cache once per pass, so it sees what changes while minimized.
        self.notifications.observe(&s.cache);
        apply_frame(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            frame,
        );
    }

    fn draw(&mut self, ui: &mut egui::Ui) {
        let minimized = ui.input(|input| input.viewport().minimized.unwrap_or(false));
        let mut effects = Vec::new();
        let mut shared = self
            .shared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for feature in shared.features.take_focus() {
            shell::focus(&mut self.dock, shell::CoreTab::Feature(feature));
        }
        let titles = shared.features.titles();
        let placeholders = shared.features.placeholders();
        let unsupported = shared.features.unsupported_notes();
        egui::Panel::bottom("core-footer")
            .resizable(false)
            .frame(egui::Frame::NONE.inner_margin(egui::vec2(6.0, 3.0)))
            .show(ui, |ui| {
                shell::footer(
                    ui,
                    shell::FooterState {
                        status: &shared.status,
                        connected: shared.connected,
                        outcome_unknown: &shared.outcome_unknown,
                        unsupported: &unsupported,
                        titles: &titles,
                        placeholders: &placeholders,
                    },
                    &mut effects,
                );
            });
        let selection = Selection {
            project: self.project.clone(),
            thread: self.selected.clone(),
        };
        self.notifications.observe(&shared.cache);
        let frame = {
            let s = &mut *shared;
            let view = FeatureView {
                connected: s.connected,
                capabilities: &s.capabilities,
                cache: &s.cache,
                selection: &selection,
            };
            let mut pass = s.features.begin_dock_pass(minimized, view);
            let mut viewer = shell::Viewer {
                pass: &mut pass,
                titles: &titles,
                conversation: &mut self.conversation,
                io: conversation::ConversationIo {
                    cache: &s.cache,
                    connected: s.connected,
                    drafts: &mut s.drafts,
                    attachment_drafts: &s.attachment_drafts,
                    sending: &s.sending,
                    loading_attachments: &s.loading_attachments,
                },
                selection: &selection,
                effects: &mut effects,
                notifications: &mut self.notifications,
            };
            egui_dock::DockArea::new(&mut self.dock)
                .style(crate::theme::dock::dock_style(ui.style()))
                .show_leaf_close_all_buttons(false)
                .show_inside(ui, &mut viewer);
            pass.finish(ui)
        };
        apply_frame(
            &self.actions,
            &mut shared,
            &mut self.project,
            &mut self.selected,
            frame,
        );
        for effect in effects {
            apply_effect(
                &self.actions,
                &mut self.dock,
                &mut self.conversation,
                &mut shared,
                effect,
            );
        }
        if shared.features.focus_pending() {
            ui.ctx().request_repaint();
        }
    }
}

fn apply_effect(
    actions: &tokio::sync::mpsc::Sender<Action>,
    dock: &mut egui_dock::DockState<shell::CoreTab>,
    conversation: &mut conversation::ConversationPane,
    shared: &mut Shared,
    effect: shell::Effect,
) {
    {
        match effect {
            shell::Effect::Send(action) => enqueue(actions, shared, *action),
            shell::Effect::Reveal(feature) => shared.features.reveal(feature),
            shell::Effect::Open(tab) => shell::focus(dock, tab),
            shell::Effect::ResetLayout => *dock = shell::default_dock(),
            shell::Effect::Status(status) => shared.status = status,
            shell::Effect::AttachRequested {
                request,
                project,
                thread,
            } => {
                *shared
                    .loading_attachments
                    .entry(thread.to_string())
                    .or_default() += 1;
                conversation
                    .attachment_imports
                    .insert(request, (project, thread));
            }
            shell::Effect::RetryAttachments { project, thread } => {
                let pending: Vec<_> = shared
                    .attachment_drafts
                    .get(thread.as_str())
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|file| file.reference.is_none() && !file.uploading && !file.uncertain)
                    .collect();
                for draft in pending {
                    enqueue(
                        actions,
                        shared,
                        Action::Upload {
                            project: project.clone(),
                            thread: thread.clone(),
                            file: draft.file,
                        },
                    );
                }
            }
            shell::Effect::DiscardAttachments(thread) => {
                for file in shared
                    .take_attachment_drafts(thread.as_str())
                    .into_iter()
                    .flatten()
                {
                    if let Some(id) = file.upload_id {
                        enqueue(
                            actions,
                            shared,
                            Action::Command {
                                command: Command::Conversation(
                                    ConversationCommand::DiscardAttachment { attachment_id: id },
                                ),
                                draft: None,
                            },
                        );
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod shell_tests;

#[cfg(test)]
mod upload_regressions {
    use super::*;
    use workbench_protocol::namespaces::ExecutionCommand;

    fn upload() -> Action {
        Action::Upload {
            project: ProjectId::new("p").unwrap(),
            thread: ThreadId::new("t").unwrap(),
            file: crate::attachments::AttachmentFile {
                bytes: b"local bytes".to_vec(),
                ..crate::fixture::attachment_draft().file
            },
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
            state
                .attachment_drafts
                .get("t")
                .is_some_and(|i| i.len() == 1),
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
        assert_eq!(state.attachment_drafts["t"][0].file.bytes, b"local bytes");
        assert!(state.attachment_drafts["t"][0].reference.is_none());
    }
    #[tokio::test]
    async fn discard_while_upload_reply_is_held_keeps_draft() {
        let (tx, mut rx) = tokio::sync::mpsc::channel(4);
        let mut state = Shared::default();
        enqueue(&tx, &mut state, upload());
        let Action::Upload { .. } = rx.recv().await.unwrap() else {
            panic!()
        };
        let (release, held) = tokio::sync::oneshot::channel();
        let reply = tokio::spawn(async move {
            held.await.unwrap();
        });
        assert!(
            state.take_attachment_drafts("t").is_none(),
            "discard must be refused until upload has a known outcome"
        );
        release.send(()).unwrap();
        reply.await.unwrap();
        assert_eq!(state.attachment_drafts["t"].len(), 1);
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
            data: Vec::new(),
            previews: Vec::new(),
            observations: Vec::new(),
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
