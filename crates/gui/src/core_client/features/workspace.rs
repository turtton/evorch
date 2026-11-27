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
mod dialogs;
mod sidebar;
mod tree;

use self::tree::RunState;
use super::{
    Availability, Feature, FeatureContext, FeatureError, FeatureId, FeatureReply, Navigation,
    Selection, Visibility,
};
use crate::core_client::cache::Cache;
use std::collections::{BTreeMap, BTreeSet};
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
    views::ProjectSummary,
};
use workspace_ui::ProjectSort;

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
    ModelChoice(workbench_protocol::commands::ModelChoice),
    Archive(bool),
    Pin(bool),
}

#[derive(Debug, Clone)]
enum Inflight {
    Families,
    Models(String, u64),
    Projects,
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
    /// The projects as last listed; until then the snapshot's are shown.
    pub projects: Option<Vec<ProjectSummary>>,
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

    /// The projects to show: the last listing, or the snapshot's before one arrived.
    pub fn project_list<'a>(&'a self, cache: &'a Cache) -> Vec<&'a ProjectSummary> {
        match &self.projects {
            Some(projects) => projects.iter().collect(),
            None => cache.projects.values().collect(),
        }
    }
}

/// How the sidebar is arranged in this window; never sent and kept across a reset.
#[derive(Debug)]
struct SidebarView {
    sort: ProjectSort,
    collapsed: BTreeSet<String>,
    /// The threads whose children the person folded; the rest show theirs. Kept in the
    /// session file, so a restart does not unfold them.
    folded_threads: BTreeSet<String>,
}

impl Default for SidebarView {
    fn default() -> Self {
        Self {
            sort: ProjectSort::Name,
            collapsed: BTreeSet::new(),
            folded_threads: BTreeSet::new(),
        }
    }
}

impl WorkspaceFeature {
    /// The threads folded in the sidebar, for the session file.
    pub fn folded_threads(&self) -> Vec<String> {
        self.view.folded_threads.iter().cloned().collect()
    }

    /// Folds the threads the session file kept.
    pub fn restore_folded_threads(&mut self, threads: impl IntoIterator<Item = String>) {
        self.view.folded_threads = threads.into_iter().collect();
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Dialog {
    AddProject,
    Settings(String),
}

/// What a frame of the sidebar asked for, applied once it is drawn.
#[derive(Debug, Default)]
struct Requests {
    navigations: Vec<Navigation>,
    asks: Vec<(String, Intent)>,
    trust: Vec<(String, TrustLevel)>,
    project: Vec<(String, ProjectIntent)>,
    register: Option<(String, String)>,
    lanes: Vec<super::lanes::LaneIntent>,
    pull_requests: Vec<super::pull_requests::PullRequestIntent>,
}

#[derive(Default)]
pub struct WorkspaceFeature {
    model: SidebarModel,
    model_catalogs: BTreeMap<String, workbench_protocol::namespaces::settings::ProjectModelChoices>,
    model_catalog_generation: u64,
    detail_generation: u64,
    detail_generations: BTreeMap<u64, u64>,
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
    /// The thread whose title is being edited, and the text typed so far.
    renaming: Option<(String, String)>,
    /// The rename field takes the keyboard focus when it next appears.
    focus_rename: bool,
    /// The pages of the family list read so far, until the last one arrives.
    family_pages: Vec<FamilyView>,
    /// The pages of the project list read so far, until the last one arrives.
    project_pages: Vec<ProjectSummary>,
    listing_projects: bool,
    view: SidebarView,
    dialog: Option<Dialog>,
    /// Threads whose run ended while another thread was shown, until they are opened.
    unread: BTreeSet<String>,
    /// What the runs of each cached thread made of it at the last pass, to see a run end.
    last_state: BTreeMap<String, RunState>,
    /// Threads with a working run at the last pass, to refuse archiving their family.
    working: BTreeSet<String>,
    /// Details and accesses that may be out of date. They stay shown until the fresh
    /// one replaces them, but nothing is decided on them.
    refetch_details: BTreeSet<String>,
    refetch_access: BTreeSet<String>,
    /// What the lanes feature can offer in a lane thread's menu; `None` while it cannot.
    lane_menu: Option<super::lanes::LaneMenu>,
    /// What the pull requests feature shows in the rows and menus; `None` while it cannot.
    pull_request_menu: Option<super::pull_requests::PullRequestMenu>,
}

impl WorkspaceFeature {
    pub(super) fn set_lane_menu(&mut self, menu: Option<super::lanes::LaneMenu>) {
        self.lane_menu = menu;
    }

    pub(super) fn set_pull_request_menu(
        &mut self,
        menu: Option<super::pull_requests::PullRequestMenu>,
    ) {
        self.pull_request_menu = menu;
    }

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
        if self.current_access(project) {
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

    pub fn model_catalog(
        &self,
        project: &str,
    ) -> Option<&workbench_protocol::namespaces::settings::ProjectModelChoices> {
        self.model_catalogs.get(project)
    }

    /// Forget the model catalogs: the applied settings changed. `Features`
    /// calls this for the settings events (the feature only receives its own
    /// namespace). Answers of requests sent earlier are dropped by generation.
    pub(super) fn invalidate_model_catalogs(&mut self) {
        self.model_catalogs.clear();
        self.model_catalog_generation += 1;
    }

    pub fn model_choice_pending(&self, thread: &str) -> bool {
        self.intents.contains_key(thread)
            || self
                .inflight
                .values()
                .any(|i| matches!(i, Inflight::Command(t) if t == thread))
            || self.refetch_details.contains(thread)
    }

    fn want_model_catalog(&mut self, project: &workbench_protocol::ids::ProjectId) {
        if self.model_catalogs.contains_key(project.as_str()) || self.inflight.values().any(|i| matches!(i, Inflight::Models(p, g) if p == project.as_str() && *g == self.model_catalog_generation)) { return; }
        let tag = self.tag(Inflight::Models(
            project.to_string(),
            self.model_catalog_generation,
        ));
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Settings(
                workbench_protocol::namespaces::settings::SettingsQuery::ModelChoices {
                    project: project.clone(),
                },
            ),
        ));
    }

    fn tag(&mut self, what: Inflight) -> u64 {
        self.next_tag += 1;
        if matches!(what, Inflight::Detail(_)) {
            self.detail_generations
                .insert(self.next_tag, self.detail_generation);
        }
        self.inflight.insert(self.next_tag, what);
        self.next_tag
    }

    /// The user asked for `intent` on `thread`. It is sent with the revision
    /// the core reports, never with one guessed here.
    pub fn ask(&mut self, thread: &str, intent: Intent) {
        self.notice = None;
        self.intents.insert(thread.to_owned(), intent);
        if self.current_detail(thread) {
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
        if self.current_access(project) {
            self.send_trust(project);
        } else {
            self.want_access(project);
        }
    }

    /// The detail of `thread` is read and not out of date.
    fn current_detail(&self, thread: &str) -> bool {
        self.model.details.contains_key(thread) && !self.refetch_details.contains(thread)
    }

    /// The access of `project` is read and not out of date.
    fn current_access(&self, project: &str) -> bool {
        self.model.access.contains_key(project) && !self.refetch_access.contains(project)
    }

    /// Asks the core for the access of `project` unless that is already on its way.
    pub fn want_access(&mut self, project: &str) {
        if self.current_access(project)
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
        self.ask_detail(thread, DETAILS_IN_FLIGHT);
    }

    /// The selection's project access and thread detail are asked for first, so the
    /// local panes and the conversation's root never queue behind other threads.
    pub fn want_selection(&mut self, selection: &Selection, cache: &Cache) {
        if let Some(project) = &selection.project {
            self.want_access(project.as_str());
            self.want_model_catalog(project);
        }
        if let Some(thread) = &selection.thread
            && cache.threads.contains_key(thread.as_str())
        {
            // One above the cap: the other threads' queries never hold this one back.
            self.ask_detail(thread.as_str(), DETAILS_IN_FLIGHT + 1);
        }
    }

    /// Asks for the details of `thread` while fewer than `cap` detail queries wait.
    fn ask_detail(&mut self, thread: &str, cap: usize) {
        let waiting = self
            .inflight
            .values()
            .filter(|f| matches!(f, Inflight::Detail(_)))
            .count();
        let known = self
            .inflight
            .values()
            .any(|f| matches!(f, Inflight::Detail(t) if t == thread));
        if known || waiting >= cap || self.current_detail(thread) {
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
            Intent::ModelChoice(choice) => WorkspaceCommand::SetThreadModelChoice(
                workbench_protocol::namespaces::workspace::SetThreadModelChoice {
                    thread: detail.thread.clone(),
                    choice,
                    expected_revision: detail.revision,
                },
            ),
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
                // The same guards as the row's Archive button, on the newest detail: a
                // pinned family or one with a working run is not archived. As in the
                // embedded workbench, such a click is ignored. Restoring is always safe.
                if archived && (detail.pinned || family.pinned || self.family_works(thread, family))
                {
                    return;
                }
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

    /// Whether `thread` or a member of `family` had a working run at the last pass.
    fn family_works(&self, thread: &str, family: &FamilyView) -> bool {
        self.working.iter().any(|member| {
            member == thread
                || self
                    .model
                    .details
                    .get(member)
                    .and_then(|detail| detail.family.as_ref())
                    .is_some_and(|key| *key == family.key)
        })
    }

    /// Asks for the family list again when the core said something changed.
    pub fn refresh_if_stale(&mut self) {
        if !self.stale || self.refreshing || self.listing_projects {
            return;
        }
        self.stale = false;
        self.refreshing = true;
        self.listing_projects = true;
        self.family_pages.clear();
        self.project_pages.clear();
        self.request_families(PageRequest::default());
        self.request_projects(PageRequest::default());
    }

    fn request_projects(&mut self, page: PageRequest) {
        let tag = self.tag(Inflight::Projects);
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::ListProjects(page)),
        ));
    }

    /// Notes which threads stopped running while another one was shown, as the
    /// embedded workbench does: a run that finished, failed or waits for the user is
    /// unread, a cancelled one is not. A thread that runs again, or is gone from the
    /// projection, is not unread any more.
    fn observe_threads(&mut self, cache: &Cache, selection: &Selection) {
        let selected = selection.thread.as_ref().map(ThreadId::as_str);
        self.working = tree::working_threads(cache)
            .into_iter()
            .map(str::to_owned)
            .collect();
        for id in cache.threads.keys() {
            let state = tree::run_state(cache, &self.model, id);
            let before = self.last_state.insert(id.clone(), state);
            match state {
                RunState::Running => {
                    self.unread.remove(id);
                }
                RunState::Settled | RunState::Failed
                    if before == Some(RunState::Running) && selected != Some(id.as_str()) =>
                {
                    self.unread.insert(id.clone());
                }
                _ => {}
            }
        }
        self.last_state
            .retain(|id, _| cache.threads.contains_key(id));
        self.unread.retain(|id| cache.threads.contains_key(id));
    }

    /// Threads whose finished work was not opened yet.
    pub fn unread(&self) -> &BTreeSet<String> {
        &self.unread
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
        if matches!(what, Inflight::Detail(_))
            && self
                .detail_generations
                .remove(&tag)
                .is_some_and(|generation| generation != self.detail_generation)
        {
            if let Inflight::Detail(thread) = &what
                && (self.intents.contains_key(thread) || self.refetch_details.contains(thread))
            {
                self.want_detail(thread);
            }
            return;
        }
        match (what, reply) {
            (
                Inflight::Models(project, generation),
                FeatureReply::Query(Ok(QueryReply::Settings(
                    workbench_protocol::namespaces::settings::SettingsReply::ModelChoices(catalog),
                ))),
            ) => {
                if generation == self.model_catalog_generation
                    && catalog.project.as_str() == project
                {
                    self.model_catalogs.insert(project, catalog);
                }
            }
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
                // What was learned about a thread before may be out of date. It stays
                // shown until the fresh read replaces it, so the tree keeps its shape
                // and a pinned or running family never looks archivable meanwhile.
                self.refetch_details = self.model.details.keys().cloned().collect();
                for thread in self.refetch_details.clone() {
                    self.want_detail(&thread);
                }
                self.refetch_access = self.model.access.keys().cloned().collect();
                for project in self.refetch_access.clone() {
                    self.want_access(&project);
                }
            }
            (
                Inflight::Projects,
                FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Projects(page)))),
            ) => {
                self.project_pages.extend(page.items);
                if let Some(cursor) = page.next_cursor {
                    self.request_projects(PageRequest {
                        cursor: Some(cursor),
                        ..PageRequest::default()
                    });
                    return;
                }
                self.listing_projects = false;
                self.model.projects = Some(std::mem::take(&mut self.project_pages));
            }
            (
                Inflight::Access(project),
                FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::ProjectAccess(
                    access,
                )))),
            ) => {
                self.refetch_access.remove(&project);
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
                        if self.dialog == Some(Dialog::AddProject) {
                            self.dialog = None;
                        }
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
                if detail.thread.as_str() != thread {
                    return;
                }
                self.refetch_details.remove(&thread);
                self.model.details.insert(thread.clone(), *detail);
                if self.intents.contains_key(&thread) {
                    self.send_intent(&thread);
                }
            }
            (Inflight::Command(thread), FeatureReply::Command(Ok(reply))) => {
                match reply.result.as_deref() {
                    Some(CommandResult::Workspace(WorkspaceResult::Applied(_))) => {
                        self.refetch_details.insert(thread.clone());
                        self.want_detail(&thread);
                        self.stale = true;
                    }
                    Some(CommandResult::Workspace(WorkspaceResult::Rejected(rejection))) => {
                        self.notice = Some(rejection_text(rejection));
                        // Whatever it was decided on is not current any more.
                        self.model.details.remove(&thread);
                        self.refetch_details.insert(thread.clone());
                        self.want_detail(&thread);
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
        if matches!(what, Inflight::Projects) {
            // The last list (or the snapshot's) stays shown, and the next change the
            // core reports lists the projects again. Asking at once would relist the
            // families and reread every detail on each failure.
            self.project_pages.clear();
            self.listing_projects = false;
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
                Inflight::Command(thread) => {
                    let thread = thread.clone();
                    self.refetch_details.insert(thread.clone());
                    self.want_detail(&thread);
                    self.stale = true;
                }
                Inflight::Register => self.stale = true,
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
        self.detail_generation += 1;
        self.model_catalogs.clear();
        self.model_catalog_generation += 1;
        self.refetch_details.clear();
        self.refetch_access.clear();
        // Replies can queue reads between frames. Keep those not-yet-sent
        // requests with their tags, otherwise want_detail/want_access would
        // wait forever for a request discarded here. A command in this outbox
        // has never left the feature; sent commands are never queued again.
        self.refreshing = false;
        self.listing_projects = false;
        // A new snapshot is not a run ending: what is unread stays, and states are
        // learned again from the snapshot.
        self.last_state.clear();
        // The tags in flight and the intent behind them stay.
        self.stale = true;
    }

    fn event(&mut self, event: &UiEvent) {
        if matches!(
            event,
            UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged { .. })
        ) {
            self.detail_generation += 1;
            self.refetch_details
                .extend(self.model.details.keys().cloned());
            self.stale = true;
        }
    }

    fn reply(&mut self, tag: u64, reply: FeatureReply) {
        self.on_reply(tag, reply);
    }

    fn as_workspace(&self) -> Option<&WorkspaceFeature> {
        Some(self)
    }

    fn as_workspace_mut(&mut self) -> Option<&mut WorkspaceFeature> {
        Some(self)
    }

    fn selection_changed(&mut self, ctx: &mut FeatureContext<'_>) {
        if let Some(thread) = &ctx.selection().thread {
            self.unread.remove(thread.as_str());
        }
    }

    fn maintain(&mut self, ctx: &mut FeatureContext<'_>, _visibility: Visibility) {
        self.observe_threads(ctx.cache(), ctx.selection());
    }

    fn render(&mut self, _ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) {
        self.refresh_if_stale();
        self.flush(ctx);
    }

    fn render_selector(&mut self, ui: &mut egui::Ui, ctx: &mut FeatureContext<'_>) -> bool {
        self.refresh_if_stale();
        self.want_selection(ctx.selection(), ctx.cache());
        let threads: Vec<String> = ctx.cache().threads.keys().cloned().collect();
        for thread in &threads {
            self.want_detail(thread);
        }
        let projects: Vec<String> = self
            .model
            .project_list(ctx.cache())
            .iter()
            .map(|project| project.id.as_str().to_owned())
            .collect();
        for project in &projects {
            self.want_access(project);
        }
        let mut requests = Requests::default();
        self.draw_sidebar(ui, ctx, &mut requests);
        if ui.is_visible() {
            // A modal is drawn over the whole window, so the hidden pass that only
            // collects requests must not draw one.
            self.draw_dialog(ui.ctx(), &mut requests);
        }
        let Requests {
            navigations,
            asks,
            trust,
            project,
            register,
            lanes,
            pull_requests,
        } = requests;
        if let Some((name, path)) = register {
            self.register_project(&name, &path);
        }
        for (project, intent) in project {
            self.ask_project(&project, intent);
        }
        for (thread, intent) in asks {
            self.ask(&thread, intent);
        }
        for (project, trust) in trust {
            self.ask_trust(&project, trust);
        }
        for navigation in navigations {
            ctx.navigate(navigation);
        }
        for intent in lanes {
            ctx.lane(intent);
        }
        for intent in pull_requests {
            ctx.pull_request(intent);
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
        commands::CommandReply,
        ids::RunId,
        namespaces::workspace::Applied,
        views::{Page, RunPhase, RunRole, RunSummary, ThreadState},
        wire::Revision,
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
            model_choice: workbench_protocol::commands::ModelChoice::RoleDefault,
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
            matches!(
                feature.take_outbox().as_slice(),
                [Outgoing::Query(
                    _,
                    Query::Workspace(WorkspaceQuery::GetThreadDetail(_))
                )]
            ),
            "only a fresh read is sent, never the conflicted command"
        );
        feature.refresh_if_stale();
        assert!(matches!(
            feature.take_outbox().as_slice(),
            [
                Outgoing::Query(_, Query::Workspace(WorkspaceQuery::ListFamilies(_))),
                Outgoing::Query(_, Query::Workspace(WorkspaceQuery::ListProjects(_)))
            ]
        ));
    }

    fn project(id: &str, name: &str) -> ProjectSummary {
        ProjectSummary {
            id: workbench_protocol::ids::ProjectId::new(id).unwrap(),
            display_name: name.into(),
            thread_count: 0,
            revision: Revision(1),
        }
    }

    fn projects_page(items: Vec<ProjectSummary>, next: Option<&str>) -> FeatureReply {
        FeatureReply::Query(Ok(QueryReply::Workspace(WorkspaceReply::Projects(Page {
            items,
            next_cursor: next
                .map(|cursor| workbench_protocol::ids::PageCursor::new(cursor).unwrap()),
            snapshot_revision: Revision(1),
        }))))
    }

    /// The tag of the only `ListProjects` query in `outbox`, and the cursor it asks from.
    fn projects_query(outbox: &[Outgoing]) -> (u64, Option<String>) {
        let mut found = outbox.iter().filter_map(|outgoing| match outgoing {
            Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::ListProjects(page))) => {
                Some((*tag, page.cursor.as_ref().map(|c| c.as_str().to_owned())))
            }
            _ => None,
        });
        let query = found.next().expect("the projects are listed");
        assert!(found.next().is_none(), "listed once: {outbox:?}");
        query
    }

    fn names(feature: &WorkspaceFeature) -> Vec<String> {
        feature
            .model
            .projects
            .iter()
            .flatten()
            .map(|p| p.display_name.clone())
            .collect()
    }

    // Given: a sidebar that has not read anything yet
    // When: it refreshes and the core lists the projects over two pages, then says the
    //       workspace changed
    // Then: both pages make the list, and the change lists them again
    #[test]
    fn refresh_lists_projects_over_every_page_and_a_workspace_change_lists_them_again() {
        let mut feature = WorkspaceFeature {
            stale: true,
            ..WorkspaceFeature::default()
        };
        feature.refresh_if_stale();
        let (tag, cursor) = projects_query(&feature.take_outbox());
        assert!(cursor.is_none());
        feature.reply(tag, projects_page(vec![project("p1", "one")], Some("next")));
        assert!(
            feature.model.projects.is_none(),
            "a first page is not the list"
        );
        let (tag, cursor) = projects_query(&feature.take_outbox());
        assert_eq!(cursor.as_deref(), Some("next"));
        feature.reply(tag, projects_page(vec![project("p2", "two")], None));
        assert_eq!(names(&feature), ["one", "two"]);

        feature.event(&UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged {
            revision: Revision(5),
        }));
        // The family listing of the first refresh is still unanswered.
        let families = feature
            .inflight
            .iter()
            .find_map(|(tag, what)| matches!(what, Inflight::Families).then_some(*tag))
            .unwrap();
        feature.reply(families, families_reply_empty());
        feature.refresh_if_stale();
        let (tag, _) = projects_query(&feature.take_outbox());
        feature.reply(tag, projects_page(vec![project("p1", "renamed")], None));
        assert_eq!(names(&feature), ["renamed"]);
    }

    fn families_reply_empty() -> FeatureReply {
        families(Vec::new())
    }

    fn summary(id: &str, state: ThreadState) -> workbench_protocol::views::ThreadSummary {
        workbench_protocol::views::ThreadSummary {
            id: thread(id),
            project: workbench_protocol::ids::ProjectId::new("p").unwrap(),
            title: id.into(),
            state,
            lead_thread: None,
            revision: Revision(1),
            updated_at: workbench_protocol::wire::UnixMillis(1),
        }
    }

    fn run(id: &str, thread_id: &str, phase: RunPhase) -> RunSummary {
        RunSummary {
            id: RunId::new(id).unwrap(),
            thread: thread(thread_id),
            parent_run: None,
            role: RunRole::Orchestrator,
            phase,
            revision: Revision(1),
        }
    }

    /// Threads with one root run each, in the given phase. The summary says Running
    /// whenever the run has not ended, as the core's does.
    fn cache_with(phases: &[(&str, RunPhase)]) -> Cache {
        let mut cache = Cache::default();
        for &(id, phase) in phases {
            let state = match phase {
                RunPhase::Done | RunPhase::Cancelled => ThreadState::Idle,
                RunPhase::Failed => ThreadState::Failed,
                _ => ThreadState::Running,
            };
            cache.threads.insert(id.into(), summary(id, state));
            let root = format!("{id}-root");
            cache.runs.insert(root.clone(), run(&root, id, phase));
        }
        cache
    }

    fn showing(thread_id: Option<&str>) -> Selection {
        Selection {
            project: Some(workbench_protocol::ids::ProjectId::new("p").unwrap()),
            thread: thread_id.map(thread),
        }
    }

    /// One composition pass as the dock runs it for a feature whose tab is hidden.
    fn pass(feature: &mut WorkspaceFeature, cache: &Cache, selection: &Selection, changed: bool) {
        let mut frame = super::super::FeatureFrame::default();
        let mut data = super::super::DataInbox::default();
        let mut ctx = FeatureContext {
            feature: FeatureId::Workspace,
            view: super::super::FeatureView {
                connected: true,
                capabilities: &[],
                cache,
                selection,
            },
            frame: &mut frame,
            data: &mut data,
        };
        if changed {
            feature.selection_changed(&mut ctx);
        }
        let hidden = Visibility {
            available: true,
            expanded: false,
            minimized: false,
        };
        feature.maintain(&mut ctx, hidden);
        assert!(frame.actions.is_empty(), "noting unread sends nothing");
    }

    // Given: two running threads while "a" is shown
    // When: both stop, then "b" is opened, then "b" runs and stops while it is shown
    // Then: only "b" becomes unread, opening it clears that, and stopping while shown
    //       leaves it read
    #[test]
    fn a_thread_that_stops_while_another_is_shown_is_unread_until_it_is_opened() {
        let mut feature = WorkspaceFeature::default();
        let running = cache_with(&[("a", RunPhase::Running), ("b", RunPhase::Running)]);
        let stopped = cache_with(&[("a", RunPhase::Done), ("b", RunPhase::Done)]);
        pass(&mut feature, &running, &showing(Some("a")), true);
        pass(&mut feature, &stopped, &showing(Some("a")), false);
        assert_eq!(feature.unread().iter().collect::<Vec<_>>(), ["b"]);

        pass(&mut feature, &stopped, &showing(Some("b")), true);
        assert!(feature.unread().is_empty());

        let b_runs = cache_with(&[("a", RunPhase::Done), ("b", RunPhase::Running)]);
        pass(&mut feature, &b_runs, &showing(Some("b")), false);
        pass(&mut feature, &stopped, &showing(Some("b")), false);
        assert!(feature.unread().is_empty());
    }

    // Given: three running threads while none of them is shown
    // When: one finishes its turn and waits for the user (the core's summary still says
    //       Running), one fails and one is cancelled
    // Then: the first two are unread and the cancelled one is not, as in the embedded
    //       workbench
    #[test]
    fn a_finished_turn_and_a_failure_are_unread_but_a_cancelled_run_is_not() {
        let mut feature = WorkspaceFeature::default();
        let selection = showing(Some("other"));
        pass(
            &mut feature,
            &cache_with(&[
                ("turn", RunPhase::Running),
                ("fail", RunPhase::Running),
                ("stop", RunPhase::Running),
            ]),
            &selection,
            true,
        );
        let ended = cache_with(&[
            ("turn", RunPhase::WaitingForUser),
            ("fail", RunPhase::Failed),
            ("stop", RunPhase::Cancelled),
        ]);
        assert_eq!(ended.threads["turn"].state, ThreadState::Running);
        pass(&mut feature, &ended, &selection, false);
        assert_eq!(
            feature.unread().iter().collect::<Vec<_>>(),
            ["fail", "turn"]
        );
    }

    // Given: a thread whose root run was stopped while a subagent of it runs on
    // When: the subagent finishes while another thread is shown
    // Then: the thread stays stopped and is not unread
    #[test]
    fn a_stopped_root_is_not_unread_when_its_subagents_finish() {
        let mut feature = WorkspaceFeature::default();
        let selection = showing(Some("other"));
        let mut cache = cache_with(&[("t", RunPhase::Cancelled)]);
        let mut subagent = run("sub", "t", RunPhase::Running);
        subagent.parent_run = Some(RunId::new("t-root").unwrap());
        cache.runs.insert("sub".into(), subagent.clone());
        pass(&mut feature, &cache, &selection, true);
        subagent.phase = RunPhase::Done;
        cache.runs.insert("sub".into(), subagent);
        pass(&mut feature, &cache, &selection, false);
        assert!(feature.unread().is_empty());
    }

    // Given: an unread thread and a running one
    // When: a new snapshot replaces the projection with both idle
    // Then: the unread thread stays unread and the snapshot invents no other
    #[test]
    fn a_reset_keeps_what_is_unread_and_invents_nothing() {
        let mut feature = WorkspaceFeature::default();
        let selection = showing(None);
        pass(
            &mut feature,
            &cache_with(&[("a", RunPhase::Running), ("b", RunPhase::Running)]),
            &selection,
            true,
        );
        pass(
            &mut feature,
            &cache_with(&[("a", RunPhase::Done), ("b", RunPhase::Running)]),
            &selection,
            false,
        );
        feature.reset();
        pass(
            &mut feature,
            &cache_with(&[("a", RunPhase::Done), ("b", RunPhase::Done)]),
            &selection,
            false,
        );
        assert_eq!(feature.unread().iter().collect::<Vec<_>>(), ["a"]);
    }

    struct Screen {
        feature: WorkspaceFeature,
        cache: Cache,
        selection: Selection,
        frame: super::super::FeatureFrame,
        data: super::super::DataInbox,
        invisible: bool,
    }

    fn draw(ui: &mut egui::Ui, screen: &mut Screen) {
        let Screen {
            feature,
            cache,
            selection,
            frame,
            data,
            invisible,
        } = screen;
        let mut ctx = FeatureContext {
            feature: FeatureId::Workspace,
            view: super::super::FeatureView {
                connected: true,
                capabilities: &[],
                cache,
                selection,
            },
            frame,
            data,
        };
        if *invisible {
            let mut hidden = ui.new_child(egui::UiBuilder::new().invisible());
            feature.render_selector(&mut hidden, &mut ctx);
        } else {
            feature.render_selector(ui, &mut ctx);
        }
    }

    fn screen(invisible: bool) -> egui_kittest::Harness<'static, Screen> {
        let mut cache = Cache::default();
        cache.projects.insert("p".into(), project("p", "evorch"));
        egui_kittest::Harness::builder()
            .with_size(egui::vec2(480.0, 640.0))
            .build_ui_state(
                draw,
                Screen {
                    feature: WorkspaceFeature::default(),
                    cache,
                    selection: showing(None),
                    frame: super::super::FeatureFrame::default(),
                    data: super::super::DataInbox::default(),
                    invisible,
                },
            )
    }

    fn registrations(screen: &Screen) -> Vec<(String, String)> {
        screen
            .frame
            .actions
            .iter()
            .filter_map(|action| match &action.request {
                super::super::FeatureRequest::Command(Command::Workspace(
                    WorkspaceCommand::RegisterProject(register),
                )) => Some((register.name.clone(), register.path.clone())),
                _ => None,
            })
            .collect()
    }

    // Given: the Projects tab
    // When: "Add project" on the toolbar is clicked and the modal is filled in and confirmed
    // Then: the core is asked to register exactly the typed name and directory
    #[test]
    fn the_toolbar_add_dialog_registers_the_typed_project() {
        use egui_kittest::kittest::Queryable;
        let mut harness = screen(false);
        harness.run();
        harness.get_by_label("Add project").click();
        harness.run();
        assert!(harness.query_by_label("New project").is_some());
        for (label, text) in [("Project name", "repo"), ("Project path", "/work/repo")] {
            harness.get_by_label(label).focus();
            harness.run();
            harness.get_by_label(label).type_text(text);
            harness.run();
        }
        harness.get_by_label("Add").click();
        harness.run();
        assert_eq!(
            registrations(harness.state()),
            [("repo".to_owned(), "/work/repo".to_owned())]
        );
    }

    // Given: a modal the user opened
    // When: the sidebar is drawn on a pass that only collects its requests
    // Then: the modal is not drawn; on a visible pass it is
    #[test]
    fn modals_are_not_drawn_on_an_invisible_ui() {
        use egui_kittest::kittest::Queryable;
        let mut harness = screen(true);
        harness.state_mut().feature.dialog = Some(Dialog::AddProject);
        harness.run();
        assert!(harness.query_by_label("New project").is_none());
        harness.state_mut().invisible = false;
        harness.run();
        assert!(harness.query_by_label("New project").is_some());
    }

    // Given: twenty threads in project p, so the first pass, which asks for p's access,
    //        leaves the detail queries at their cap
    // When: the last of the threads is selected
    // Then: the next pass asks for its detail anyway
    #[test]
    fn the_selected_thread_is_asked_for_past_the_other_threads() {
        let mut harness = screen(true);
        let ids: Vec<String> = (0..20).map(|n| format!("t{n:02}")).collect();
        for id in &ids {
            harness
                .state_mut()
                .cache
                .threads
                .insert(id.clone(), summary(id, ThreadState::Idle));
        }
        harness.run();
        let asked = |screen: &Screen| -> Vec<Query> {
            screen
                .frame
                .actions
                .iter()
                .filter_map(|action| match &action.request {
                    super::super::FeatureRequest::Query(query) => Some(query.clone()),
                    super::super::FeatureRequest::Command(_) => None,
                })
                .collect()
        };
        let details = |queries: &[Query]| -> Vec<String> {
            queries
                .iter()
                .filter_map(|query| match query {
                    Query::Workspace(WorkspaceQuery::GetThreadDetail(target)) => {
                        Some(target.thread.as_str().to_owned())
                    }
                    _ => None,
                })
                .collect()
        };
        let first = asked(harness.state());
        assert!(first.iter().any(|query| matches!(
            query,
            Query::Workspace(WorkspaceQuery::GetProjectAccess(target)) if target.project.as_str() == "p"
        )));
        assert_eq!(details(&first).len(), DETAILS_IN_FLIGHT);
        assert!(!details(&first).contains(&ids[19]));

        harness.state_mut().frame.actions.clear();
        harness.state_mut().selection = showing(Some(&ids[19]));
        harness.run();

        assert_eq!(details(&asked(harness.state())), [ids[19].clone()]);
    }

    /// Lists the families once and answers with `items`, leaving the project listing
    /// unanswered.
    fn list_families(feature: &mut WorkspaceFeature, items: Vec<FamilyView>) {
        feature.stale = true;
        feature.refresh_if_stale();
        let tag = feature
            .take_outbox()
            .into_iter()
            .find_map(|outgoing| match outgoing {
                Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::ListFamilies(_))) => {
                    Some(tag)
                }
                _ => None,
            })
            .expect("the families are listed");
        feature.reply(tag, families(items));
    }

    fn detail_queries(outbox: &[Outgoing]) -> Vec<(u64, String)> {
        outbox
            .iter()
            .filter_map(|outgoing| match outgoing {
                Outgoing::Query(tag, Query::Workspace(WorkspaceQuery::GetThreadDetail(target))) => {
                    Some((*tag, target.thread.as_str().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    // Given: a pinned thread whose detail was read
    // When: the core reports a change, the families are listed again, and the user asks
    //       to archive the thread before and after its detail is read again
    // Then: the old detail stays shown while the fresh one is read, nothing is decided
    //       on the old one, and the pinned family is not archived
    #[test]
    fn a_family_refresh_keeps_details_shown_and_never_archives_a_pinned_family() {
        let mut feature = WorkspaceFeature::default();
        feature.model.details.insert(
            "t1".into(),
            ThreadDetail {
                pinned: true,
                ..detail("t1", 1, "t1")
            },
        );
        list_families(&mut feature, vec![family("t1", 5, 7)]);
        assert!(feature.model.pinned("t1"), "the pin stays shown meanwhile");
        let reads = detail_queries(&feature.take_outbox());
        let [(tag, id)] = reads.as_slice() else {
            panic!("{reads:?}")
        };
        assert_eq!(id, "t1");

        feature.ask("t1", Intent::Archive(true));
        assert!(
            feature.take_outbox().is_empty(),
            "nothing is decided on an out-of-date detail"
        );
        feature.reply(
            *tag,
            detail_reply(ThreadDetail {
                pinned: true,
                ..detail("t1", 2, "t1")
            }),
        );
        assert!(feature.take_outbox().is_empty(), "a pinned family stays");
        assert!(feature.intents.is_empty(), "the refused click is dropped");
    }

    // Given: a family whose fork runs
    // When: archiving its root is asked, then again once the run ended
    // Then: the first is ignored and the second is sent
    #[test]
    fn a_family_with_a_working_run_is_not_archived() {
        let mut feature = WorkspaceFeature::default();
        feature
            .model
            .families
            .insert("root".into(), family("root", 5, 7));
        for id in ["root", "fork"] {
            feature
                .model
                .details
                .insert(id.into(), detail(id, 1, "root"));
        }
        let selection = showing(None);
        let mut cache = cache_with(&[("root", RunPhase::Done), ("fork", RunPhase::Running)]);
        pass(&mut feature, &cache, &selection, true);
        feature.ask("root", Intent::Archive(true));
        assert!(feature.take_outbox().is_empty());

        cache.runs.get_mut("fork-root").unwrap().phase = RunPhase::Done;
        pass(&mut feature, &cache, &selection, false);
        feature.ask("root", Intent::Archive(true));
        let (_, command) = only_command(feature.take_outbox());
        assert!(matches!(command, WorkspaceCommand::SetFamilyArchived(a) if a.archived));
    }

    // Given: a refresh whose project listing fails
    // When: the family listing completes
    // Then: nothing is listed again until the core reports a change, and the snapshot's
    //       projects stay shown
    #[test]
    fn a_failed_project_listing_waits_for_the_next_change() {
        let mut feature = WorkspaceFeature {
            stale: true,
            ..WorkspaceFeature::default()
        };
        feature.refresh_if_stale();
        let outbox = feature.take_outbox();
        let (projects, _) = projects_query(&outbox);
        let Some(Outgoing::Query(families_tag, _)) = outbox.first() else {
            panic!("{outbox:?}")
        };
        feature.reply(
            projects,
            FeatureReply::Query(Err(FeatureError::new("storage failed"))),
        );
        feature.reply(*families_tag, families_reply_empty());
        feature.refresh_if_stale();
        assert!(feature.take_outbox().is_empty(), "no relist loop");
        assert!(feature.settled() && feature.model.projects.is_none());

        feature.event(&UiEvent::Workspace(WorkspaceEvent::WorkspaceChanged {
            revision: Revision(2),
        }));
        feature.refresh_if_stale();
        projects_query(&feature.take_outbox());
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
        let reads = feature.take_outbox();
        let [Outgoing::Query(fresh, Query::Workspace(WorkspaceQuery::GetThreadDetail(_)))] =
            reads.as_slice()
        else {
            panic!("{reads:?}")
        };
        assert!(feature.model.details.is_empty());
        feature.reply(*fresh, detail_reply(detail("t2", 10, "t2")));
        let (_, command) = only_command(feature.take_outbox());
        assert!(
            matches!(command, WorkspaceCommand::RenameThread(r) if r.expected_revision == Revision(10))
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
        assert!(matches!(
            feature.take_outbox().as_slice(),
            [Outgoing::Query(
                _,
                Query::Workspace(WorkspaceQuery::GetThreadDetail(_))
            )]
        ));
    }

    #[test]
    fn model_choice_saves_with_revision_and_waits_for_a_fresh_authoritative_detail() {
        use workbench_protocol::commands::{ModelChoice, ModelPreference};
        let choice = ModelChoice::Explicit(ModelPreference {
            profile: "work".into(),
            model: None,
            reasoning_effort: Some("custom".into()),
        });
        for outcome in ["applied", "conflict", "unknown"] {
            let mut feature = WorkspaceFeature::default();
            feature
                .model
                .details
                .insert("t1".into(), detail("t1", 1, "t1"));
            feature.ask("t1", Intent::ModelChoice(choice.clone()));
            assert!(feature.model_choice_pending("t1"));
            assert!(!feature.model_choice_pending("t2"));
            let (tag, command) = only_command(feature.take_outbox());
            let WorkspaceCommand::SetThreadModelChoice(command) = command else {
                panic!()
            };
            assert_eq!(command.choice, choice);
            assert_eq!(command.expected_revision, Revision(1));
            let reply = match outcome {
                "applied" => command_reply(WorkspaceResult::Applied(
                    workbench_protocol::namespaces::workspace::Applied {
                        revision: Revision(2),
                        changed: true,
                        project: None,
                    },
                )),
                "conflict" => {
                    command_reply(WorkspaceResult::Rejected(WorkspaceRejection::Conflict {
                        expected: Revision(1),
                        actual: Revision(2),
                    }))
                }
                _ => FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
            };
            feature.on_reply(tag, reply);
            assert!(feature.model_choice_pending("t1"));
            let reads = feature.take_outbox();
            let [Outgoing::Query(read_tag, Query::Workspace(WorkspaceQuery::GetThreadDetail(_)))] =
                reads.as_slice()
            else {
                panic!("{reads:?}")
            };
            let mut current = detail("t1", 2, "t1");
            current.model_choice = choice.clone();
            feature.on_reply(*read_tag, detail_reply(current));
            assert!(!feature.model_choice_pending("t1"));
            assert_eq!(feature.model.details["t1"].model_choice, choice);
            assert!(feature.take_outbox().is_empty());
        }
    }

    // Given: a cached model catalog in the workspace feature of `Features`
    // When: a settings snapshot change or a finished model reload arrives (the
    //       latter with the user settings revision unchanged)
    // Then: the catalog is dropped and an answer requested before is ignored;
    //       a finished save does not drop it
    #[test]
    fn settings_events_invalidate_the_model_catalog_through_features() {
        use crate::core_client::features::Features;
        use workbench_protocol::ids::{OperationId, ProjectId};
        use workbench_protocol::namespaces::settings::{
            ProjectModelChoices, RoleModelChoice, SettingsEvent, SettingsOperationKind,
            SettingsOperationView, SettingsReply, SettingsRevision,
        };
        use workbench_protocol::operation::{OperationState, OperationView};
        let operation = |kind, state| {
            UiEvent::Settings(SettingsEvent::SettingsOperationCompleted {
                operation: SettingsOperationView {
                    operation: OperationView {
                        operation_id: OperationId::new("op-1").unwrap(),
                        state,
                        revision: Revision(1),
                        result: None,
                    },
                    kind,
                    revision: None,
                    message: None,
                },
            })
        };
        let role = RoleModelChoice {
            model: "m".into(),
            reasoning_effort: None,
        };
        let project = ProjectId::new("p").unwrap();
        let catalog = ProjectModelChoices {
            project: project.clone(),
            profiles: Vec::new(),
            worker: role.clone(),
            orchestrator: role,
        };
        let revision = SettingsRevision::new("a".repeat(64)).unwrap();
        let events = [
            UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged {
                revision: revision.clone(),
                applied_revision: Some(revision),
            }),
            operation(SettingsOperationKind::ReloadModels, OperationState::Ready),
        ];
        for event in &events {
            let mut features = Features::default();
            let workspace = features.workspace_mut().unwrap();
            workspace.model_catalogs.insert("p".into(), catalog.clone());
            workspace.model_catalogs.remove("p");
            workspace.want_model_catalog(&project);
            let stale_tag = match workspace.take_outbox().as_slice() {
                [Outgoing::Query(tag, _)] => *tag,
                other => panic!("{other:?}"),
            };
            workspace.model_catalogs.insert("p".into(), catalog.clone());
            features.event(event);
            let workspace = features.workspace_mut().unwrap();
            assert!(workspace.model_catalog("p").is_none(), "{event:?}");
            workspace.on_reply(
                stale_tag,
                FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::ModelChoices(
                    catalog.clone(),
                )))),
            );
            assert!(workspace.model_catalog("p").is_none(), "{event:?}");
        }
        let mut features = Features::default();
        let workspace = features.workspace_mut().unwrap();
        workspace.model_catalogs.insert("p".into(), catalog);
        features.event(&operation(
            SettingsOperationKind::SaveDocument,
            OperationState::Ready,
        ));
        assert!(features.workspace().unwrap().model_catalog("p").is_some());
    }

    #[test]
    fn old_detail_and_catalog_results_cannot_overwrite_reconnected_state() {
        use workbench_protocol::ids::ProjectId;
        use workbench_protocol::namespaces::settings::{
            ProjectModelChoices, RoleModelChoice, SettingsReply,
        };
        let mut feature = WorkspaceFeature::default();
        feature.want_detail("t1");
        let old_detail = match feature.take_outbox().as_slice() {
            [Outgoing::Query(tag, _)] => *tag,
            other => panic!("{other:?}"),
        };
        let project = ProjectId::new("p").unwrap();
        feature.want_model_catalog(&project);
        let old_catalog = match feature.take_outbox().as_slice() {
            [Outgoing::Query(tag, _)] => *tag,
            other => panic!("{other:?}"),
        };
        feature.reset();
        feature
            .model
            .details
            .insert("t1".into(), detail("t1", 5, "t1"));
        feature.on_reply(old_detail, detail_reply(detail("t1", 1, "t1")));
        assert_eq!(feature.model.details["t1"].revision, Revision(5));
        let role = RoleModelChoice {
            model: "old".into(),
            reasoning_effort: None,
        };
        feature.on_reply(
            old_catalog,
            FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::ModelChoices(
                ProjectModelChoices {
                    project,
                    profiles: Vec::new(),
                    worker: role.clone(),
                    orchestrator: role,
                },
            )))),
        );
        assert!(feature.model_catalog("p").is_none());
    }
}
