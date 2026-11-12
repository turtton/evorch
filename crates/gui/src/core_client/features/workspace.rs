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
            FamilyView, RenameThread, SetFamilyArchived, SetFamilyPinned, ThreadDetail,
            WorkspaceCommand, WorkspaceRejection, WorkspaceResult,
        },
    },
    queries::{PageRequest, Query, QueryReply},
};

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
    outbox: Vec<Outgoing>,
    /// The core says something changed (or nothing was read yet).
    stale: bool,
    refreshing: bool,
    notice: Option<String>,
    renaming: Option<(String, String)>,
}

impl WorkspaceFeature {
    pub fn model(&self) -> &SidebarModel {
        &self.model
    }

    pub fn notice(&self) -> Option<&str> {
        self.notice.as_deref()
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

    fn want_detail(&mut self, thread: &str) {
        let waiting = self
            .inflight
            .values()
            .filter(|f| matches!(f, Inflight::Detail(_)))
            .count();
        let known = self
            .inflight
            .values()
            .any(|f| matches!(f, Inflight::Detail(t) if t == thread));
        if known || waiting >= DETAILS_IN_FLIGHT {
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
        let (Some(intent), Some(detail)) = (
            self.intents.remove(thread),
            self.model.details.get(thread).cloned(),
        ) else {
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
        let tag = self.tag(Inflight::Families);
        self.outbox.push(Outgoing::Query(
            tag,
            Query::Workspace(WorkspaceQuery::ListFamilies(PageRequest::default())),
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
                self.refreshing = false;
                self.model.families = page
                    .items
                    .into_iter()
                    .map(|f| (f.key.to_string(), f))
                    .collect();
                // What was learned about a thread before may be out of date.
                let known: Vec<String> = self.model.details.keys().cloned().collect();
                self.model.details.clear();
                for thread in known {
                    self.want_detail(&thread);
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
            self.refreshing = false;
            self.stale = true;
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
        self.outbox.clear();
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
        let mut navigations = Vec::new();
        let mut asks: Vec<(String, Intent)> = Vec::new();
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
            if ui.small_button("New conversation").clicked() {
                navigations.push(Navigation::NewConversation(project.clone()));
            }
            let mut visible: Vec<_> = threads
                .iter()
                .filter(|t| &t.project == project && !self.model.archived(t.id.as_str()))
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
                    let selected = selection.thread.as_ref() == Some(&thread.id);
                    if ui
                        .selectable_label(selected, format!("{pinned}{}", thread.title))
                        .clicked()
                    {
                        navigations.push(Navigation::SelectThread {
                            project: project.clone(),
                            thread: thread.id.clone(),
                        });
                    }
                    if ui.small_button("Rename").clicked() {
                        self.renaming = Some((id.to_owned(), thread.title.clone()));
                    }
                    let pin = !self.model.pinned(id);
                    if ui.small_button(if pin { "Pin" } else { "Unpin" }).clicked() {
                        asks.push((id.to_owned(), Intent::Pin(pin)));
                    }
                    if ui.small_button("Archive").clicked() {
                        asks.push((id.to_owned(), Intent::Archive(true)));
                    }
                });
            }
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(ui.visuals().warn_fg_color, notice);
        }
        for (thread, intent) in asks {
            self.ask(&thread, intent);
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
