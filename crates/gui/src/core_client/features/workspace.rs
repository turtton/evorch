//! Desktop feature of the `workspace` area: the sidebar of projects and
//! threads, drawn from what the core says (ADR 0045, PR 09).
//!
//! The core is the writer of the shared sidebar. This feature holds no
//! authority over it: it renders the projection the client already has, asks
//! the core for the details a command needs (the revision it is decided on),
//! sends typed commands and shows what the core answered. A rejection, in
//! particular a conflict with another client, is shown as such and nothing is
//! assumed applied. Window selection and anything the user has not sent stay
//! client state (`Selection`, drafts).
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Navigation,
};
use std::collections::BTreeMap;
use workbench_protocol::{
    commands::{Command, CommandResult},
    events::UiEvent,
    handshake::Capability,
    ids::ThreadId,
    namespaces::{
        WorkspaceEvent, WorkspaceQuery, WorkspaceReply,
        workspace::{
            AllowedDirectoryAction, FamilyIdentity, FamilyView, ProjectAccessView, RegisterProject,
            RenameProject, RenameThread, SetFamilyArchived, SetFamilyPinned, SetProjectTrust,
            ThreadDetail, TrustLevel, UpdateAllowedDirectory, WorkspaceCommand, WorkspaceRejection,
            WorkspaceResult,
        },
    },
    queries::{PageRequest, Query, QueryReply},
};

fn family_identity(family: &FamilyView) -> FamilyIdentity {
    FamilyIdentity {
        key: family.key.clone(),
        membership_epoch: family.membership_epoch,
    }
}

/// How many detail queries may wait for an answer at once.
const DETAILS_IN_FLIGHT: usize = 16;

/// What the user asked for on a thread, to be sent once its revision is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Rename(String),
    Archive(bool),
    Pin(bool),
}

#[derive(Debug, Clone)]
enum Inflight {
    Families,
    Detail(String),
    Command(String),
    Access(String),
    TrustCommand(String),
    ProjectCommand(String),
    Register,
}

/// What the user asked for on a project's access, to be sent once its revision is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectIntent {
    Rename(String),
    AddDirectory { path: String, trust: TrustLevel },
    RemoveDirectory(String),
}

/// The text the project forms hold between frames.
#[derive(Debug, Default)]
struct ProjectForms {
    register_name: String,
    register_path: String,
    rename: String,
    directory: String,
    directory_approved: bool,
}

/// A request waiting to leave with the next frame.
#[derive(Debug, Clone)]
pub enum Outgoing {
    Command(u64, Command),
    Query(u64, Query),
}

/// What the sidebar knows about the core's answer; all of it disposable.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SidebarModel {
    pub families: BTreeMap<String, FamilyView>,
    pub details: BTreeMap<String, ThreadDetail>,
    /// Trust and allowed directories of the projects asked about.
    pub access: BTreeMap<String, ProjectAccessView>,
}

impl SidebarModel {
    /// The family of `thread`, once both its detail and the family list are known.
    pub fn family_of(&self, thread: &str) -> Option<&FamilyView> {
        let key = self.details.get(thread)?.family.as_ref()?;
        self.families.get(key.as_str())
    }

    pub fn archived(&self, thread: &str) -> bool {
        self.details.get(thread).is_some_and(|d| d.archived)
    }

    pub fn pinned(&self, thread: &str) -> bool {
        self.details.get(thread).is_some_and(|d| d.pinned)
    }
}

#[derive(Default)]
pub struct WorkspaceFeature {
    model: SidebarModel,
    next_tag: u64,
    /// Which request each tag stands for. Kept across a reset: a reply to a
    /// request sent before it is still delivered.
    inflight: BTreeMap<u64, Inflight>,
    intents: BTreeMap<String, Intent>,
    trust_intents: BTreeMap<String, TrustLevel>,
    project_intents: BTreeMap<String, ProjectIntent>,
    forms: ProjectForms,
    /// What the last project operation did, once the core said it was applied.
    confirmation: Option<String>,
    outbox: Vec<Outgoing>,
    /// The core says something changed (or nothing was read yet).
    stale: bool,
    refreshing: bool,
    notice: Option<String>,
    renaming: Option<(String, String)>,
    /// The pages of the family list read so far, until the last one arrives.
    family_pages: Vec<FamilyView>,
    /// Archived conversations are listed (with a way to restore them).
    show_archived: bool,
}

impl WorkspaceFeature {
    pub fn model(&self) -> &SidebarModel {
        &self.model
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
    }

    /// The outcome of the last project operation, when the core applied it.
    pub fn confirmation(&self) -> Option<&str> {
        self.confirmation.as_deref()
    }

    /// The user asked to register the directory `path` as a project named `name`. The core
    /// canonicalises the path and refuses one it does not accept; nothing is guessed here.
    pub fn register_project(&mut self, name: &str, path: &str) {
        self.notice = None;
        self.confirmation = None;
        let (name, path) = (name.trim(), path.trim());
        if name.is_empty() || path.is_empty() {
            self.notice = Some("the project needs a name and a directory".into());
            return;
        }
        let tag = self.tag(Inflight::Register);
        self.outbox.push(Outgoing::Command(
            tag,
            Command::Workspace(WorkspaceCommand::RegisterProject(RegisterProject {
                name: name.to_owned(),
                path: path.to_owned(),
            })),
        ));
    }

    /// The user asked for `intent` on `project`; it is sent with the revision the core
    /// reports for the project's access, never with one guessed here.
    pub fn ask_project(&mut self, project: &str, intent: ProjectIntent) {
        self.notice = None;
        self.confirmation = None;
        self.project_intents.insert(project.to_owned(), intent);
        if self.model.access.contains_key(project) {
            self.send_project_intent(project);
        } else {
            self.want_access(project);
        }
    }

    fn send_project_intent(&mut self, project: &str) {
        let (Some(intent), Some(access)) = (
            self.project_intents.remove(project),
            self.model.access.get(project).cloned(),
        ) else {
            return;
        };
        let command = match intent {
            ProjectIntent::Rename(name) => WorkspaceCommand::RenameProject(RenameProject {
                project: access.project,
                name,
                expected_revision: access.revision,
            }),
            ProjectIntent::AddDirectory { path, trust } => {
                WorkspaceCommand::UpdateAllowedDirectory(UpdateAllowedDirectory {
                    project: access.project,
                    action: AllowedDirectoryAction::Add { path, trust },
                    expected_revision: access.revision,
                })
            }
            ProjectIntent::RemoveDirectory(path) => {
                WorkspaceCommand::UpdateAllowedDirectory(UpdateAllowedDirectory {
                    project: access.project,
                    action: AllowedDirectoryAction::Remove { path },
                    expected_revision: access.revision,
                })
            }
        };
        let tag = self.tag(Inflight::ProjectCommand(project.to_owned()));
        self.outbox
            .push(Outgoing::Command(tag, Command::Workspace(command)));
    }

    /// Whether the sidebar is waiting for an answer of the core.
    pub fn settled(&self) -> bool {
        !self.stale && self.inflight.is_empty() && self.outbox.is_empty()
    }

    fn tag(&mut self, what: Inflight) -> u64 {
        self.next_tag += 1;
        self.inflight.insert(self.next_tag, what);
        self.next_tag
    }

    /// The user asked for `intent` on `thread`. It is sent with the revision
    /// the core reports, never with one guessed here.
    pub fn ask(&mut self, thread: &str, intent: Intent) {
        self.notice = None;
        self.intents.insert(thread.to_owned(), intent);
        if self.model.details.contains_key(thread) {
            self.send_intent(thread);
        } else {
            self.want_detail(thread);
        }
    }

    /// The user asked for `trust` on `project`; it is sent with the revision
    /// the core reports for the project's access.
    pub fn ask_trust(&mut self, project: &str, trust: TrustLevel) {
        self.notice = None;
        self.trust_intents.insert(project.to_owned(), trust);
        if self.model.access.contains_key(project) {
            self.send_trust(project);
        } else {
            self.want_access(project);
        }
    }

    /// Asks the core for the access of `project` unless that is already on its way.
    pub fn want_access(&mut self, project: &str) {
        if self.model.access.contains_key(project)
            || self
                .inflight
                .values()
                .any(|f| matches!(f, Inflight::Access(p) if p == project))
        {
            return;
        }
        let Ok(id) = workbench_protocol::ids::ProjectId::new(project) else {
            return;
        };
        let tag = self.tag(Inflight::Access(project.to_owned()));
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::GetProjectAccess(
                workbench_protocol::namespaces::workspace::ProjectTarget { project: id },
            )),
        ));
    }

    fn send_trust(&mut self, project: &str) {
        let (Some(trust), Some(access)) = (
            self.trust_intents.remove(project),
            self.model.access.get(project).cloned(),
        ) else {
            return;
        };
        let tag = self.tag(Inflight::TrustCommand(project.to_owned()));
        self.outbox.push(Outgoing::Command(
            tag,
            Command::Workspace(WorkspaceCommand::SetProjectTrust(SetProjectTrust {
                project: access.project,
                trust,
                expected_revision: access.revision,
            })),
        ));
    }

    /// Asks for the details of `thread` unless they are known or on their way.
    pub fn want_detail(&mut self, thread: &str) {
        let waiting = self
            .inflight
            .values()
            .filter(|f| matches!(f, Inflight::Detail(_)))
            .count();
        let known = self
            .inflight
            .values()
            .any(|f| matches!(f, Inflight::Detail(t) if t == thread));
        if known || waiting >= DETAILS_IN_FLIGHT || self.model.details.contains_key(thread) {
            return;
        }
        let Ok(id) = ThreadId::new(thread) else {
            return;
        };
        let tag = self.tag(Inflight::Detail(thread.to_owned()));
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::GetThreadDetail(
                workbench_protocol::commands::ThreadTarget { thread: id },
            )),
        ));
    }

    fn send_intent(&mut self, thread: &str) {
        // The intent stays until the detail it is decided on is known.
        let Some(detail) = self.model.details.get(thread).cloned() else {
            return;
        };
        let Some(intent) = self.intents.remove(thread) else {
            return;
        };
        let command = match intent {
            Intent::Rename(title) => WorkspaceCommand::RenameThread(RenameThread {
                thread: detail.thread.clone(),
                title,
                expected_revision: detail.revision,
            }),
            Intent::Archive(archived) => {
                let Some(family) = self.model.family_of(thread) else {
                    self.intents
                        .insert(thread.to_owned(), Intent::Archive(archived));
                    self.stale = true;
                    return;
                };
                WorkspaceCommand::SetFamilyArchived(SetFamilyArchived {
                    thread: detail.thread.clone(),
                    archived,
                    expected_family: family_identity(family),
                    expected_revision: family.archive_revision,
                })
            }
            Intent::Pin(pinned) => {
                let Some(family) = self.model.family_of(thread) else {
                    self.intents.insert(thread.to_owned(), Intent::Pin(pinned));
                    self.stale = true;
                    return;
                };
                WorkspaceCommand::SetFamilyPinned(SetFamilyPinned {
                    thread: detail.thread.clone(),
                    pinned,
                    expected_family: family_identity(family),
                    expected_revision: family.pin_revision,
                })
            }
        };
        let tag = self.tag(Inflight::Command(thread.to_owned()));
        self.outbox
            .push(Outgoing::Command(tag, Command::Workspace(command)));
    }

    /// Asks for the family list again when the core said something changed.
    pub fn refresh_if_stale(&mut self) {
        if !self.stale || self.refreshing {
            return;
        }
        self.stale = false;
        self.refreshing = true;
        self.family_pages.clear();
        self.request_families(PageRequest::default());
    }

    fn request_families(&mut self, page: PageRequest) {
        let tag = self.tag(Inflight::Families);
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::ListFamilies(page)),
        ));
    }

    /// What is waiting to be sent, in order.
    pub fn take_outbox(&mut self) -> Vec<Outgoing> {
        std::mem::take(&mut self.outbox)
    }

    fn on_reply(&mut self, tag: u64, reply: FeatureReply) {
        let Some(what) = self.inflight.remove(&tag) else {
            return;
        };
        match (what, reply) {
            (
                Inflight::Families,
                FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Families(page)))),
            ) => {
                self.family_pages.extend(page.items);
                if let Some(cursor) = page.next_cursor {
                    // The list goes on: follow the cursor to the last page.
                    self.request_families(PageRequest {
                        cursor: Some(cursor),
                        ..PageRequest::default()
                    });
                    return;
                }
                self.refreshing = false;
                self.model.families = std::mem::take(&mut self.family_pages)
                    .into_iter()
                    .map(|f| (f.key.to_string(), f))
                    .collect();
                // What was learned about a thread before may be out of date.
                let known: Vec<String> = self.model.details.keys().cloned().collect();
                self.model.details.clear();
                for thread in known {
                    self.want_detail(&thread);
                }
                let known: Vec<String> = self.model.access.keys().cloned().collect();
                self.model.access.clear();
                for project in known {
                    self.want_access(&project);
                }
            }
            (
                Inflight::Access(project),
                FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ProjectAccess(
                    access,
                )))),
            ) => {
                self.model.access.insert(project.clone(), *access);
                if self.trust_intents.contains_key(&project) {
                    self.send_trust(&project);
                }
                if self.project_intents.contains_key(&project) {
                    self.send_project_intent(&project);
                }
            }
            (Inflight::ProjectCommand(project), FeatureReply::Command(Ok(reply))) => {
                match reply.result.as_deref() {
                    Some(CommandResult::Workspace(WorkspaceResult::Applied(applied))) => {
                        self.confirmation = Some(if applied.changed {
                            "the project settings were saved".into()
                        } else {
                            "the project already had these settings".into()
                        });
                    }
                    Some(CommandResult::Workspace(WorkspaceResult::Rejected(rejection))) => {
                        self.notice = Some(rejection_text(rejection));
                    }
                    _ => self.notice = Some("the core answered something unexpected".into()),
                }
                // What it was decided on is read again, applied or not.
                self.model.access.remove(&project);
                self.stale = true;
                self.want_access(&project);
            }
            (Inflight::Register, FeatureReply::Command(Ok(reply))) => {
                match reply.result.as_deref() {
                    Some(CommandResult::Workspace(WorkspaceResult::Applied(applied))) => {
                        self.confirmation = Some(if applied.changed {
                            "the project was registered".into()
                        } else {
                            "the directory was already registered".into()
                        });
                        self.forms.register_name.clear();
                        self.forms.register_path.clear();
                        self.stale = true;
                    }
                    Some(CommandResult::Workspace(WorkspaceResult::Rejected(rejection))) => {
                        self.notice = Some(rejection_text(rejection));
                    }
                    _ => self.notice = Some("the core answered something unexpected".into()),
                }
            }
            (Inflight::TrustCommand(project), FeatureReply::Command(Ok(reply))) => {
                match reply.result.as_deref() {
                    Some(CommandResult::Workspace(WorkspaceResult::Applied(_))) => {
                        self.model.access.remove(&project);
                        self.stale = true;
                        self.want_access(&project);
                    }
                    Some(CommandResult::Workspace(WorkspaceResult::Rejected(rejection))) => {
                        self.notice = Some(rejection_text(rejection));
                        self.model.access.remove(&project);
                        self.stale = true;
                        self.want_access(&project);
                    }
                    _ => self.notice = Some("the core answered something unexpected".into()),
                }
            }
            (
                Inflight::Detail(thread),
                FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ThreadDetail(
                    detail,
                )))),
            ) => {
                self.model.details.insert(thread.clone(), *detail);
                if self.intents.contains_key(&thread) {
                    self.send_intent(&thread);
                }
            }
            (Inflight::Command(thread), FeatureReply::Command(Ok(reply))) => {
                match reply.result.as_deref() {
                    Some(CommandResult::Workspace(WorkspaceResult::Applied(_))) => {
                        self.stale = true;
                    }
                    Some(CommandResult::Workspace(WorkspaceResult::Rejected(rejection))) => {
                        self.notice = Some(rejection_text(rejection));
                        // Whatever it was decided on is not current any more.
                        self.model.details.remove(&thread);
                        self.stale = true;
                    }
                    _ => self.notice = Some("the core answered something unexpected".into()),
                }
            }
            (what, FeatureReply::Query(Err(error)) | FeatureReply::Command(Err(error))) => {
                self.failed(&what, &error);
            }
            _ => self.notice = Some("the core answered something unexpected".into()),
        }
    }

    fn failed(&mut self, what: &Inflight, error: &FeatureError) {
        if matches!(what, Inflight::Families) {
            self.family_pages.clear();
            self.refreshing = false;
            self.stale = true;
        }
        if error.outcome_unknown {
            // The command may have been applied: what it changed is read again,
            // and it is never sent again.
            match what {
                Inflight::ProjectCommand(project) | Inflight::TrustCommand(project) => {
                    let project = project.clone();
                    self.model.access.remove(&project);
                    self.stale = true;
                    self.want_access(&project);
                }
                Inflight::Register | Inflight::Command(_) => self.stale = true,
                _ => {}
            }
        }
        self.notice = Some(if error.outcome_unknown {
            format!("not known whether it was applied: {}", error.message)
        } else {
            error.message.clone()
        });
    }
}

fn rejection_text(rejection: &WorkspaceRejection) -> String {
    match rejection {
        WorkspaceRejection::Conflict { .. } => {
            "changed elsewhere meanwhile; look again and retry".into()
        }
        WorkspaceRejection::Tombstoned => "the conversation was deleted".into(),
        WorkspaceRejection::NotFound => "no longer exists".into(),
        WorkspaceRejection::FamilyChanged { reason } => reason.clone(),
        WorkspaceRejection::InvalidPath { reason }
        | WorkspaceRejection::Invalid { reason }
        | WorkspaceRejection::PermissionDenied { reason }
        | WorkspaceRejection::StorageFailed { reason } => reason.clone(),
        WorkspaceRejection::Busy => "another process is saving; try again".into(),
    }
}

impl Feature for WorkspaceFeature {
    fn id(&self) -> FeatureId {
        FeatureId::Workspace
    }

    fn title(&self) -> &'static str {
        "Workspace"
    }

    fn availability(&self) -> Availability {
        Availability::Implemented {
            required: &[Capability::Commands, Capability::Queries],
        }
    }

    fn reset(&mut self) {
        self.model = SidebarModel::default();
        // Replies can queue reads between frames. Keep those not-yet-sent
        // requests with their tags, otherwise want_detail/want_access would
        // wait forever for a request discarded here. A command in this outbox
        // has never left the feature; sent commands are never queued again.
        self.refreshing = false;
        // The tags in flight and the intent behind them stay.
        self.stale = true;
    }

    fn event(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged { .. })
        ) {
            self.stale = true;
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        self.on_reply(tag, reply);
    }

    fn as_workspace(&self) -> Option<&WorkspaceFeature> {
        Some(self)
    }

    fn render(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.refresh_if_stale();
        if let Some(notice) = &self.notice {
            ui.colored_label(ui.visuals().warn_fg_color, notice);
        }
        self.flush(ctx);
    }

    fn render_selector(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) -> bool {
        self.refresh_if_stale();
        let selection = ctx.selection().clone();
        let projects: Vec<_> = ctx.cache().projects.values().cloned().collect();
        let threads: Vec<_> = ctx.cache().threads.values().cloned().collect();
        for thread in &threads {
            self.want_detail(thread.id.as_str());
        }
        if let Some(project) = selection.project.as_ref() {
            self.want_access(project.as_str());
        }
        let mut navigations = Vec::new();
        let mut asks: Vec<(String, Intent)> = Vec::new();
        let mut trust_asks: Vec<(String, TrustLevel)> = Vec::new();
        let mut project_asks: Vec<(String, ProjectIntent)> = Vec::new();
        let mut register_ask: Option<(String, String)> = None;
        egui::CollapsingHeader::new("Register project")
            .id_salt("workspace-register-project")
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Name");
                    ui.text_edit_singleline(&mut self.forms.register_name);
                });
                ui.horizontal(|ui| {
                    ui.label("Directory");
                    ui.text_edit_singleline(&mut self.forms.register_path);
                });
                if ui.button("Register").clicked() {
                    register_ask = Some((
                        self.forms.register_name.clone(),
                        self.forms.register_path.clone(),
                    ));
                }
            });
        ui.horizontal_wrapped(|ui| {
            for project in &projects {
                let selected = selection.project.as_ref() == Some(&project.id);
                if ui
                    .selectable_label(selected, &project.display_name)
                    .clicked()
                {
                    navigations.push(Navigation::SelectProject(project.id.clone()));
                }
            }
        });
        if let Some(project) = selection.project.as_ref() {
            if let Some(access) = self.model.access.get(project.as_str()) {
                let approved = access.trust == TrustLevel::Approved;
                ui.horizontal(|ui| {
                    ui.label(if approved {
                        "Trust: Approved"
                    } else {
                        "Trust: Unapproved"
                    });
                    let (button, next) = if approved {
                        ("Revoke trust", TrustLevel::Unapproved)
                    } else {
                        ("Approve project", TrustLevel::Approved)
                    };
                    if ui.small_button(button).clicked() {
                        trust_asks.push((project.as_str().to_owned(), next));
                    }
                });
            } else {
                // Same height as the row it stands in for, so that a read in
                // progress does not move the rows below it under the pointer.
                ui.horizontal(|ui| {
                    ui.label("Trust: …");
                    ui.add_enabled(false, egui::Button::new("…").small());
                });
            }
            // The header is drawn whether or not the access was read yet, so a read that ends
            // does not move the rows below it under the pointer.
            let access = self.model.access.get(project.as_str()).cloned();
            egui::CollapsingHeader::new("Project settings")
                .id_salt(("workspace-project-settings", project.as_str()))
                .show(ui, |ui| {
                    let Some(access) = &access else {
                        ui.label("…");
                        return;
                    };
                    ui.label(format!("Directory: {}", access.root));
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut self.forms.rename)
                                .hint_text(access.name.as_str()),
                        );
                        let name = self.forms.rename.trim().to_owned();
                        if ui
                            .add_enabled(!name.is_empty(), egui::Button::new("Rename project"))
                            .clicked()
                        {
                            project_asks
                                .push((project.as_str().to_owned(), ProjectIntent::Rename(name)));
                            self.forms.rename.clear();
                        }
                    });
                    ui.label("Allowed directories");
                    for directory in &access.allowed_directories {
                        ui.horizontal(|ui| {
                            ui.label(format!(
                                "{} ({}{})",
                                directory.path,
                                if directory.trust == TrustLevel::Approved {
                                    "approved"
                                } else {
                                    "unapproved"
                                },
                                if directory.live { "" } else { ", missing" }
                            ));
                            if ui.small_button("Remove").clicked() {
                                project_asks.push((
                                    project.as_str().to_owned(),
                                    ProjectIntent::RemoveDirectory(directory.path.clone()),
                                ));
                            }
                        });
                    }
                    ui.horizontal(|ui| {
                        ui.text_edit_singleline(&mut self.forms.directory);
                        ui.checkbox(&mut self.forms.directory_approved, "Approved");
                        let path = self.forms.directory.trim().to_owned();
                        if ui
                            .add_enabled(!path.is_empty(), egui::Button::new("Add directory"))
                            .clicked()
                        {
                            let trust = if self.forms.directory_approved {
                                TrustLevel::Approved
                            } else {
                                TrustLevel::Unapproved
                            };
                            project_asks.push((
                                project.as_str().to_owned(),
                                ProjectIntent::AddDirectory { path, trust },
                            ));
                            self.forms.directory.clear();
                        }
                    });
                });
            if let Some(thread) = selection.thread.as_ref() {
                // Read from the core; changing the list is not available in this window yet.
                let dirs = self.model.details.get(thread.as_str()).map_or_else(
                    || "…".to_owned(),
                    |detail| {
                        if detail.extra_read_dirs.is_empty() {
                            "none".to_owned()
                        } else {
                            detail
                                .extra_read_dirs
                                .iter()
                                .map(|directory| directory.path.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        }
                    },
                );
                ui.label(format!(
                    "Extra read directories: {dirs} (read-only; not supported here yet)"
                ));
            }
            ui.horizontal(|ui| {
                if ui.small_button("New conversation").clicked() {
                    navigations.push(Navigation::NewConversation(project.clone()));
                }
                ui.checkbox(&mut self.show_archived, "Show archived");
            });
            let mut visible: Vec<_> = threads
                .iter()
                .filter(|t| {
                    &t.project == project
                        && (self.show_archived || !self.model.archived(t.id.as_str()))
                })
                .collect();
            visible.sort_by_key(|t| {
                (
                    !self.model.pinned(t.id.as_str()),
                    std::cmp::Reverse(t.updated_at),
                )
            });
            for thread in visible {
                let id = thread.id.as_str();
                ui.horizontal(|ui| {
                    let title = self
                        .model
                        .details
                        .get(id)
                        .map_or(thread.title.as_str(), |d| d.title.as_str())
                        .to_owned();
                    if let Some((editing, text)) = self.renaming.as_mut()
                        && editing == id
                    {
                        let done = ui.text_edit_singleline(text).lost_focus();
                        if done {
                            asks.push((id.to_owned(), Intent::Rename(text.clone())));
                            self.renaming = None;
                        }
                        return;
                    }
                    let pinned = if self.model.pinned(id) { "📌 " } else { "" };
                    let archived = if self.model.archived(id) {
                        "(archived) "
                    } else {
                        ""
                    };
                    let selected = selection.thread.as_ref() == Some(&thread.id);
                    if ui
                        .selectable_label(selected, format!("{pinned}{archived}{title}"))
                        .clicked()
                    {
                        navigations.push(Navigation::SelectThread {
                            project: project.clone(),
                            thread: thread.id.clone(),
                        });
                    }
                    if ui.small_button("Rename").clicked() {
                        self.renaming = Some((id.to_owned(), title.clone()));
                    }
                    let pin = !self.model.pinned(id);
                    if ui.small_button(if pin { "Pin" } else { "Unpin" }).clicked() {
                        asks.push((id.to_owned(), Intent::Pin(pin)));
                    }
                    if self.model.archived(id) {
                        if ui.small_button("Restore").clicked() {
                            asks.push((id.to_owned(), Intent::Archive(false)));
                        }
                    } else if ui.small_button("Archive").clicked() {
                        asks.push((id.to_owned(), Intent::Archive(true)));
                    }
                });
            }
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(ui.visuals().warn_fg_color, notice);
        }
        if let Some(confirmation) = &self.confirmation {
            ui.label(confirmation);
        }
        if let Some((name, path)) = register_ask {
            self.register_project(&name, &path);
        }
        for (project, intent) in project_asks {
            self.ask_project(&project, intent);
        }
        for (thread, intent) in asks {
            self.ask(&thread, intent);
        }
        for (project, trust) in trust_asks {
            self.ask_trust(&project, trust);
        }
        for navigation in navigations {
            ctx.navigate(navigation);
        }
        self.flush(ctx);
        true
    }
}

impl WorkspaceFeature {
    fn flush(&mut self, ctx: &mut FeatureContext<'_>) {
        for outgoing in self.take_outbox() {
            match outgoing {
                Outgoing::Command(tag, command) => ctx.command(tag, command),
                Outgoing::Query(tag, query) => ctx.query(tag, query),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workbench_protocol::{
        commands::CommandReply, namespaces::workspace::Applied, views::Page, wire::Revision,
    };

    fn thread(id: &str) -> ThreadId {
        ThreadId::new(id).unwrap()
    }

    fn detail(id: &str, revision: u64, family: &str) -> ThreadDetail {
        ThreadDetail {
            thread: thread(id),
            project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
            title: id.into(),
            family: Some(thread(family)),
            parent_thread: None,
            lineage: None,
            root_run: None,
            active_root: None,
            model_profile: None,
            chat_role: None,
            extra_read_dirs: Vec::new(),
            archived: false,
            pinned: false,
            revision: Revision(revision),
        }
    }

    fn family(key: &str, archive: u64, pin: u64) -> FamilyView {
        FamilyView {
            key: thread(key),
            membership_epoch: 0,
            members: 1,
            archived: false,
            archived_at: None,
            archive_revision: Revision(archive),
            pinned: false,
            pin_revision: Revision(pin),
        }
    }

    fn families(items: Vec<FamilyView>) -> FeatureReply {
        FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Families(Page {
            items,
            next_cursor: None,
            snapshot_revision: Revision(1),
        }))))
    }

    fn detail_reply(detail: ThreadDetail) -> FeatureReply {
        FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ThreadDetail(
            Box::new(detail),
        ))))
    }

    fn command_reply(result: WorkspaceResult) -> FeatureReply {
        FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Workspace(
            result,
        ))))
    }

    fn only_command(outbox: Vec<Outgoing>) -> (u64, WorkspaceCommand) {
        match outbox.as_slice() {
            [Outgoing::Command(tag, Command::Workspace(command))] => (*tag, command.clone()),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_rename_is_sent_with_the_revision_the_core_reported() {
        let mut feature = WorkspaceFeature::default();
        feature.ask("t1", Intent::Rename("new".into()));
        let outbox = feature.take_outbox();
        let [Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetThreadDetail(target)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        assert_eq!(target.thread, thread("t1"));
        feature.reply(*tag, detail_reply(detail("t1", 42, "t1")));
        let (_, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::RenameThread(rename) = command else {
            panic!("{command:?}")
        };
        assert_eq!(
            (rename.title.as_str(), rename.expected_revision),
            ("new", Revision(42))
        );
    }

    #[test]
    fn trust_is_sent_with_the_revision_of_the_access_the_core_reported() {
        let mut feature = WorkspaceFeature::default();
        feature.ask_trust("p", TrustLevel::Approved);
        let outbox = feature.take_outbox();
        let [Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetProjectAccess(_)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        let access = ProjectAccessView {
            project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
            name: "p".into(),
            root: "/p".into(),
            trust: TrustLevel::Unapproved,
            allowed_directories: Vec::new(),
            revision: Revision(9),
        };
        feature.on_reply(
            *tag,
            FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ProjectAccess(
                Box::new(access),
            )))),
        );
        let outbox = feature.take_outbox();
        let [Outgoing::Command(_, Command::Workspace(WorkspaceCommand::SetProjectTrust(c)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        assert_eq!(c.trust, TrustLevel::Approved);
        assert_eq!(c.expected_revision, Revision(9));
    }

    fn access_reply(revision: u64) -> FeatureReply {
        FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ProjectAccess(
            Box::new(ProjectAccessView {
                project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
                name: "p".into(),
                root: "/p".into(),
                trust: TrustLevel::Unapproved,
                allowed_directories: Vec::new(),
                revision: Revision(revision),
            }),
        ))))
    }

    fn applied(changed: bool) -> WorkspaceResult {
        WorkspaceResult::Applied(Applied {
            revision: Revision(2),
            changed,
            project: None,
        })
    }

    // Given: the project forms of the sidebar
    // When: a project is registered and the core applies it, and then refuses another
    // Then: the command carries exactly the typed name and directory, the applied
    //       result is confirmed, and the refusal is shown with the core's reason
    #[test]
    fn registering_a_project_sends_its_name_and_directory_and_shows_the_outcome() {
        let mut feature = WorkspaceFeature::default();
        feature.register_project(" repo ", " /work/repo ");
        let (tag, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::RegisterProject(register) = command else {
            panic!("a registration was expected");
        };
        assert_eq!(
            (register.name.as_str(), register.path.as_str()),
            ("repo", "/work/repo")
        );
        feature.on_reply(tag, command_reply(applied(true)));
        assert_eq!(feature.confirmation(), Some("the project was registered"));
        assert!(feature.notice().is_none());

        feature.register_project("again", "relative");
        let (tag, _) = only_command(feature.take_outbox());
        feature.on_reply(
            tag,
            command_reply(WorkspaceResult::Rejected(WorkspaceRejection::InvalidPath {
                reason: "the path is not absolute".into(),
            })),
        );
        assert_eq!(feature.notice(), Some("the path is not absolute"));
        assert!(feature.confirmation().is_none());
        assert!(feature.take_outbox().is_empty(), "nothing is sent twice");

        feature.register_project("", "/x");
        assert!(
            feature.take_outbox().is_empty(),
            "an empty name is not sent"
        );
        assert!(feature.notice().is_some());
    }

    // Given: a project whose access the core reported at revision 9
    // When: it is renamed, a directory is added and removed
    // Then: each command quotes revision 9; a refusal is shown with its reason and the
    //       access is read again; a lost reply is read back, not sent again
    #[test]
    fn project_rename_and_allowed_directories_quote_the_revision_and_show_the_outcome() {
        let mut feature = WorkspaceFeature::default();
        feature.ask_project("p", ProjectIntent::Rename("renamed".into()));
        let outbox = feature.take_outbox();
        let [Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetProjectAccess(_)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        feature.on_reply(*tag, access_reply(9));
        let (tag, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::RenameProject(rename) = command else {
            panic!("a rename was expected");
        };
        assert_eq!(
            (rename.name.as_str(), rename.expected_revision),
            ("renamed", Revision(9))
        );
        feature.on_reply(tag, command_reply(applied(true)));
        assert_eq!(
            feature.confirmation(),
            Some("the project settings were saved")
        );
        // The access is read again after the command, never patched.
        let outbox = feature.take_outbox();
        let [Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetProjectAccess(_)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        feature.on_reply(*tag, access_reply(10));

        feature.ask_project(
            "p",
            ProjectIntent::AddDirectory {
                path: "/data".into(),
                trust: TrustLevel::Approved,
            },
        );
        let (tag, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::UpdateAllowedDirectory(update) = command else {
            panic!("a directory update was expected");
        };
        assert_eq!(update.expected_revision, Revision(10));
        assert_eq!(
            update.action,
            AllowedDirectoryAction::Add {
                path: "/data".into(),
                trust: TrustLevel::Approved
            }
        );
        feature.on_reply(
            tag,
            command_reply(WorkspaceResult::Rejected(WorkspaceRejection::InvalidPath {
                reason: "the directory is nested in another".into(),
            })),
        );
        assert_eq!(feature.notice(), Some("the directory is nested in another"));
        assert!(feature.confirmation().is_none());
        let outbox = feature.take_outbox();
        let [Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetProjectAccess(_)))] =
            outbox.as_slice()
        else {
            panic!("{outbox:?}")
        };
        feature.on_reply(*tag, access_reply(11));

        feature.ask_project("p", ProjectIntent::RemoveDirectory("/data".into()));
        let (tag, command) = only_command(feature.take_outbox());
        assert!(matches!(
            command,
            WorkspaceCommand::UpdateAllowedDirectory(UpdateAllowedDirectory {
                action: AllowedDirectoryAction::Remove { .. },
                expected_revision: Revision(11),
                ..
            })
        ));
        feature.on_reply(
            tag,
            FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
        );
        assert!(feature.notice().unwrap().contains("not known"));
        let outbox = feature.take_outbox();
        assert!(
            matches!(
                outbox.as_slice(),
                [Outgoing::Query(
                    _,
                    Query::Workspace(WorkspaceQuery::GetProjectAccess(_))
                )]
            ),
            "the access is read back and nothing is sent again: {outbox:?}"
        );
    }

    #[test]
    fn what_is_known_is_not_asked_again_while_the_sidebar_draws() {
        let mut feature = WorkspaceFeature::default();
        feature.want_detail("t1");
        let tag = match feature.take_outbox().as_slice() {
            [Outgoing::Query(tag, _)] => *tag,
            other => panic!("{other:?}"),
        };
        feature.on_reply(tag, detail_reply(detail("t1", 1, "t1")));
        feature.want_detail("t1");
        assert!(feature.take_outbox().is_empty());
    }

    #[test]
    fn archive_and_pin_use_the_counters_of_the_family() {
        let mut feature = WorkspaceFeature {
            stale: true,
            ..WorkspaceFeature::default()
        };
        feature.refresh_if_stale();
        let Outgoing::Query(families_tag, _) = feature.take_outbox().remove(0) else {
            panic!()
        };
        feature.reply(families_tag, families(vec![family("root", 5, 7)]));
        feature.ask("fork", Intent::Archive(true));
        let Outgoing::Query(detail_tag, _) = feature.take_outbox().remove(0) else {
            panic!()
        };
        feature.reply(detail_tag, detail_reply(detail("fork", 1, "root")));
        let (_, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::SetFamilyArchived(archive) = command else {
            panic!("{command:?}")
        };
        assert_eq!(archive.expected_revision, Revision(5));
        assert_eq!(archive.thread, thread("fork"));

        feature.ask("fork", Intent::Pin(true));
        let (_, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::SetFamilyPinned(pin) = command else {
            panic!("{command:?}")
        };
        assert_eq!(pin.expected_revision, Revision(7));
    }

    #[test]
    fn every_page_of_the_family_list_is_read() {
        let mut feature = WorkspaceFeature {
            stale: true,
            ..WorkspaceFeature::default()
        };
        feature.refresh_if_stale();
        let Outgoing::Query(first, Query::Workspace(WorkspaceQuery::ListFamilies(page))) =
            feature.take_outbox().remove(0)
        else {
            panic!()
        };
        assert!(page.cursor.is_none());
        let cursor = workbench_protocol::ids::PageCursor::new("next").unwrap();
        feature.reply(
            first,
            FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Families(Page {
                items: (0..50).map(|n| family(&format!("a{n}"), 0, 0)).collect(),
                next_cursor: Some(cursor.clone()),
                snapshot_revision: Revision(1),
            })))),
        );
        // The first page does not settle the model: the next one is asked for.
        assert!(feature.model.families.is_empty());
        let Outgoing::Query(second, Query::Workspace(WorkspaceQuery::ListFamilies(page))) =
            feature.take_outbox().remove(0)
        else {
            panic!()
        };
        assert_eq!(page.cursor, Some(cursor));
        feature.reply(
            second,
            families((0..3).map(|n| family(&format!("b{n}"), 0, 0)).collect()),
        );
        assert_eq!(feature.model.families.len(), 53);
        assert!(feature.take_outbox().is_empty());
    }

    #[test]
    fn an_archived_family_is_restored_with_the_archive_counter_and_identity() {
        let mut feature = WorkspaceFeature::default();
        let mut view = family("root", 5, 7);
        view.membership_epoch = 3;
        view.archived = true;
        feature.model.families.insert("root".into(), view);
        let mut archived = detail("t1", 1, "root");
        archived.archived = true;
        feature.model.details.insert("t1".into(), archived);
        feature.ask("t1", Intent::Archive(false));
        let (_, command) = only_command(feature.take_outbox());
        let WorkspaceCommand::SetFamilyArchived(restore) = command else {
            panic!("{command:?}")
        };
        assert!(!restore.archived);
        assert_eq!(restore.expected_revision, Revision(5));
        assert_eq!(restore.expected_family.membership_epoch, 3);
        assert_eq!(restore.expected_family.key, thread("root"));
    }

    #[test]
    fn an_intent_survives_until_its_detail_is_known() {
        let mut feature = WorkspaceFeature::default();
        feature.intents.insert("t1".into(), Intent::Pin(true));
        feature.send_intent("t1");
        assert!(feature.take_outbox().is_empty());
        assert!(feature.intents.contains_key("t1"));
    }

    #[test]
    fn a_conflict_is_shown_and_refetched_and_never_retried_blindly() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature.ask("t1", Intent::Rename("x".into()));
        let (tag, _) = only_command(feature.take_outbox());
        feature.reply(
            tag,
            command_reply(WorkspaceResult::Rejected(WorkspaceRejection::Conflict {
                expected: Revision(1),
                actual: Revision(2),
            })),
        );
        assert!(feature.notice().is_some());
        assert!(
            feature.model.details.is_empty(),
            "the stale decision basis is dropped"
        );
        assert!(
            feature.take_outbox().is_empty(),
            "nothing is resent on its own"
        );
        feature.refresh_if_stale();
        assert!(matches!(
            feature.take_outbox().as_slice(),
            [Outgoing::Query(
                _,
                Query::Workspace(WorkspaceQuery::ListFamilies(_))
            )]
        ));
    }

    #[test]
    fn an_applied_command_and_a_core_event_both_ask_for_a_fresh_read() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature.ask("t1", Intent::Rename("x".into()));
        let (tag, _) = only_command(feature.take_outbox());
        feature.reply(
            tag,
            command_reply(WorkspaceResult::Applied(Applied {
                revision: Revision(2),
                changed: true,
                project: None,
            })),
        );
        assert!(feature.stale, "an applied command asks for a fresh read");
        feature.stale = false;
        feature.event(&UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged {
            revision: Revision(3),
        }));
        assert!(feature.stale && !feature.settled());
    }

    #[test]
    fn a_reset_drops_what_came_from_the_core_but_not_what_a_tag_stands_for() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature
            .model
            .families
            .insert("t1".into(), family("t1", 0, 0));
        feature.ask("t2", Intent::Rename("n".into()));
        let tag = match feature.take_outbox().remove(0) {
            Outgoing::Query(tag, _) => tag,
            other => panic!("{other:?}"),
        };
        feature.reset();
        assert!(feature.model.details.is_empty() && feature.model.families.is_empty());
        feature.reply(tag, detail_reply(detail("t2", 9, "t2")));
        let (_, command) = only_command(feature.take_outbox());
        assert!(
            matches!(command, WorkspaceCommand::RenameThread(r) if r.expected_revision == Revision(9))
        );
    }

    #[test]
    fn reconnect_between_a_family_reply_and_the_next_frame_does_not_lose_reads() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature.want_access("p1");
        let access_tag = match feature.take_outbox().remove(0) {
            Outgoing::Query(tag, _) => tag,
            other => panic!("{other:?}"),
        };
        feature.reply(
            access_tag,
            FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ProjectAccess(
                Box::new(ProjectAccessView {
                    project: workbench_protocol::ids::ProjectId::new("p1").unwrap(),
                    name: "repo".into(),
                    root: "/repo".into(),
                    trust: TrustLevel::Approved,
                    allowed_directories: vec![],
                    revision: Revision(1),
                }),
            )))),
        );
        feature.stale = true;
        feature.refresh_if_stale();
        let tag = match feature.take_outbox().remove(0) {
            Outgoing::Query(tag, _) => tag,
            other => panic!("{other:?}"),
        };
        // The family reply queues detail/access reads for the next frame.
        feature.reply(tag, families(vec![family("t1", 0, 0)]));
        feature.reset(); // A reconnect snapshot arrives before that frame.
        feature.want_detail("t1");
        feature.want_access("p1");
        let reads = feature.take_outbox();
        assert_eq!(
            reads.len(),
            2,
            "every pending read must still be sent: {reads:?}"
        );
        assert!(reads.iter().any(|r| matches!(
            r,
            Outgoing::Query(_, Query::Workspace(WorkspaceQuery::GetThreadDetail(_)))
        )));
        assert!(reads.iter().any(|r| matches!(
            r,
            Outgoing::Query(_, Query::Workspace(WorkspaceQuery::GetProjectAccess(_)))
        )));
    }

    #[test]
    fn a_reset_keeps_an_unsent_command_once_without_replaying_a_sent_one() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature.ask("t1", Intent::Rename("new title".into()));
        feature.reset();
        feature.reset();
        let (_, command) = only_command(feature.take_outbox());
        assert!(matches!(command, WorkspaceCommand::RenameThread(r) if r.title == "new title"));
        feature.reset();
        assert!(feature.take_outbox().is_empty());
    }

    #[test]
    fn a_lost_reply_is_reported_as_unknown_and_not_resent() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 1, "t1"));
        feature.ask("t1", Intent::Rename("x".into()));
        let (tag, _) = only_command(feature.take_outbox());
        feature.reply(
            tag,
            FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
        );
        assert!(feature.notice().unwrap().contains("not known"));
        assert!(feature.take_outbox().is_empty());
    }
}
