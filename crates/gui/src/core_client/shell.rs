//! The window frame of the core client, laid out as the embedded workbench: a dock with
//! the projects on the left, the conversation in the middle and the areas of the core in
//! tabs around it, over a footer with the menus.
use std::borrow::Cow;
use std::collections::BTreeSet;

use egui::{RichText, Ui};
use egui_dock::{DockState, NodeIndex, TabIndex, TabViewer};
use workbench_protocol::ids::{ProjectId, ThreadId};

use super::attention::TabAttention;
use super::connection::Action;
use super::conversation::{self, ConversationIo, ConversationPane};
use super::features::settings::SettingsIntent;
use super::features::{DockPass, FeatureId, Navigation, Selection};
use super::local::{self, LocalPanes, files::FileTabId};
use super::notifications::{self, NotificationLog};
use super::run_tabs::{RunPane, RunTabId, RunTabs};
use crate::panes::conversation_plan::plan_panel;
use crate::panes::goal_widgets::GoalAction;
use crate::panes::sidebar_widgets::{INDENT, ProjectRowView, project_row};
use crate::panes::tab_attention::{self, PaneAttention};
use crate::theme::icons;
use crate::theme::text::{muted, section};
use crate::theme::tokens::*;
use crate::theme::widgets::{
    accessible, compact_row, empty_state, ghost_icon_button, row_title, status_dot,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CoreTab {
    Projects,
    Conversation,
    Notifications,
    /// The selected thread's plan, opened from the composer band.
    Plan,
    Feature(FeatureId),
    /// One run's transcript, opened from an Execution row.
    Run(RunTabId),
    /// The changes in the selected thread's working tree.
    Diff,
    /// The selected project's shell.
    Terminal,
    /// A read-only preview of a local file, opened from a link.
    File(FileTabId),
}

/// What a frame asks the screen to do once the dock is drawn and the shared state is free.
pub enum Effect {
    Send(Box<Action>),
    ModelChoice(ThreadId, workbench_protocol::commands::ModelChoice),
    Reveal(FeatureId),
    Open(CoreTab),
    /// Open pane pressed on an Execution row.
    OpenRun(RunPane),
    /// A goal control pressed in the composer band, for the execution feature.
    Goal(GoalAction),
    /// A control pressed on the lane card of a lane's conversation, for the lanes feature.
    Lane(super::features::lanes::LaneCardAction),
    /// A control pressed on the pull request bar of a conversation, for the pull requests
    /// feature.
    PullRequest(super::features::pull_requests::PullRequestBarAction),
    /// "Link PR" or "Ignore" pressed on a suggestion shown on a tool card, for the pull
    /// requests feature.
    PullRequestChip(super::features::pull_requests::PullRequestChipAction),
    ResetLayout,
    ThemeSettings,
    /// ⚙ → Model Settings or Config file.
    Settings(SettingsIntent),
    Status(String),
    AttachRequested {
        request: String,
        project: ProjectId,
        thread: ThreadId,
    },
    RetryAttachments {
        project: ProjectId,
        thread: ThreadId,
    },
    /// Remove pressed on one pending file of a thread.
    RemoveAttachment {
        thread: ThreadId,
        id: String,
    },
}

impl Effect {
    pub fn send(action: Action) -> Self {
        Self::Send(Box::new(action))
    }
}

const LEFT: [FeatureId; 3] = [FeatureId::Lanes, FeatureId::Memory, FeatureId::Improvement];
const CENTRE: [FeatureId; 1] = [FeatureId::Browser];
const RIGHT_TOP: [FeatureId; 2] = [FeatureId::Execution, FeatureId::Teams];
const RIGHT_BOTTOM: [FeatureId; 2] = [FeatureId::Artifacts, FeatureId::PullRequests];

const SIDEBAR_FRACTION: f32 = 0.30;
/// The conversation's share of the centre column above the Terminal, as in the
/// embedded workbench.
const TERMINAL_FRACTION: f32 = 0.7;

/// The tabs of each leaf of the default layout: the projects, the notifications and the
/// local panes below them, the conversation, the two areas on the right and the Terminal.
/// A closed tab of a leaf reopens beside the tabs it had there.
pub fn default_leaves() -> [Vec<CoreTab>; 6] {
    let with = |head: &[CoreTab], features: &[FeatureId]| {
        let mut tabs = head.to_vec();
        tabs.extend(features.iter().copied().map(CoreTab::Feature));
        tabs
    };
    [
        vec![CoreTab::Projects],
        with(&[CoreTab::Notifications, CoreTab::Diff], &LEFT),
        with(&[CoreTab::Conversation], &CENTRE),
        with(&[], &RIGHT_TOP),
        with(&[], &RIGHT_BOTTOM),
        vec![CoreTab::Terminal],
    ]
}

pub fn default_dock() -> DockState<CoreTab> {
    let [
        projects,
        bottom_left,
        centre,
        right_top,
        right_bottom,
        terminal,
    ] = default_leaves();
    let mut dock = DockState::new(centre);
    let surface = dock.main_surface_mut();
    let [middle, left] = surface.split_left(NodeIndex::root(), SIDEBAR_FRACTION, projects);
    surface.split_below(left, 0.65, bottom_left);
    let [conversation, right] = surface.split_right(middle, 0.7, right_top);
    surface.split_below(right, 0.5, right_bottom);
    let [_, terminal] = surface.split_below(conversation, TERMINAL_FRACTION, terminal);
    // Folded to its tab bar until opened, as in the embedded workbench: no shell starts
    // before the person asks for one, and the conversation keeps its height.
    if let Ok(leaf) = surface.leaf_mut(terminal) {
        leaf.collapsed = true;
    }
    dock
}

/// Bring `tab` forward. A tab not in the dock goes back to its leaf of the default
/// layout, beside the nearest of that leaf's tabs still in the dock; a tab without one
/// opens at the end of the conversation's leaf.
pub fn focus(dock: &mut DockState<CoreTab>, tab: CoreTab) {
    if dock.find_tab(&tab).is_none() && !place_home(dock, tab) {
        match dock.find_tab(&CoreTab::Conversation) {
            Some(conversation) => {
                dock.set_focused_node_and_surface(conversation.node_path());
                dock.push_to_focused_leaf(tab);
            }
            None => dock.push_to_first_leaf(tab),
        }
    }
    if let Some(path) = dock.find_tab(&tab) {
        let _ = dock.set_active_tab(path);
        dock.set_focused_node_and_surface(path.node_path());
        if let Ok(leaf) = dock.leaf_mut(path.node_path()) {
            leaf.collapsed = false;
        }
    }
}

/// Put `tab`, not in the dock, back in its default leaf: after the nearest tab before it
/// there that is in the dock, else before the nearest one after it. Returns false when
/// the tab has no default leaf or none of the leaf's other tabs is in the dock.
fn place_home(dock: &mut DockState<CoreTab>, tab: CoreTab) -> bool {
    let Some(group) = default_leaves()
        .into_iter()
        .find(|tabs| tabs.contains(&tab))
    else {
        return false;
    };
    let at = group.iter().position(|other| *other == tab).unwrap_or(0);
    let before = group[..at].iter().rev().map(|anchor| (anchor, true));
    let after = group[at + 1..].iter().map(|anchor| (anchor, false));
    for (anchor, after) in before.chain(after) {
        if insert_near(dock, tab, *anchor, after) {
            return true;
        }
    }
    false
}

/// Insert `tab` right after (or before) `anchor` in the anchor's leaf, leaving the tab in
/// front of the leaf as it was. Returns false, inserting nothing, when `anchor` is not in
/// the dock.
fn insert_near(dock: &mut DockState<CoreTab>, tab: CoreTab, anchor: CoreTab, after: bool) -> bool {
    let Some(path) = dock.find_tab(&anchor) else {
        return false;
    };
    insert_in_leaf(
        dock,
        path.node_path(),
        Some(path.tab.0 + usize::from(after)),
        tab,
    );
    true
}

/// Insert `tab` at `index` of the leaf at `path` (its end when `None`), leaving the tab
/// in front as it was.
fn insert_in_leaf(
    dock: &mut DockState<CoreTab>,
    path: egui_dock::NodePath,
    index: Option<usize>,
    tab: CoreTab,
) {
    if let Ok(leaf) = dock.leaf_mut(path) {
        let index = index.map_or(leaf.tabs.len(), |index| index.min(leaf.tabs.len()));
        leaf.tabs.insert(index, tab);
        if leaf.active.0 >= index && leaf.tabs.len() > 1 {
            leaf.active = TabIndex(leaf.active.0 + 1);
        }
    }
}

/// Bring a local pane forward. A closed Diff opens beside the notifications, and a
/// closed Terminal below the conversation again, where the default layout has them.
pub fn open_local(dock: &mut DockState<CoreTab>, tab: CoreTab) {
    if tab == CoreTab::Terminal
        && dock.find_tab(&tab).is_none()
        && let Some(conversation) = dock.find_tab(&CoreTab::Conversation)
    {
        dock.split(
            conversation.node_path(),
            egui_dock::Split::Below,
            TERMINAL_FRACTION,
            egui_dock::Node::leaf(tab),
        );
    }
    focus_beside(dock, tab, CoreTab::Notifications);
}

/// Put a tab not in the dock right after `anchor` in the anchor's leaf (else at the
/// end of the conversation's leaf), leaving the tab in front of each leaf as it was.
pub fn place_beside(dock: &mut DockState<CoreTab>, tab: CoreTab, anchor: CoreTab) {
    if dock.find_tab(&tab).is_some() || insert_near(dock, tab, anchor, true) {
        return;
    }
    match dock.find_tab(&CoreTab::Conversation) {
        Some(path) => insert_in_leaf(dock, path.node_path(), None, tab),
        None => dock.push_to_first_leaf(tab),
    }
}

/// Bring `tab` forward. A tab not in the dock opens right after `anchor` in the
/// anchor's leaf (else beside the conversation), and the leaf's tab bar scrolls
/// back to its start so the new tab is in sight.
pub fn focus_beside(dock: &mut DockState<CoreTab>, tab: CoreTab, anchor: CoreTab) {
    place_beside(dock, tab, anchor);
    focus(dock, tab);
    if let Some(path) = dock.find_tab(&tab)
        && let Ok(leaf) = dock.leaf_mut(path.node_path())
    {
        leaf.scroll = 0.0;
    }
}

pub fn feature_icon(feature: FeatureId) -> &'static str {
    match feature {
        FeatureId::Workspace => icons::FOLDER_SIMPLE,
        FeatureId::Mcp => icons::PLUGS,
        FeatureId::Execution => icons::ROBOT,
        FeatureId::Teams => icons::USERS_THREE,
        FeatureId::Lanes => icons::STACK,
        FeatureId::Settings => icons::FILE_TEXT,
        FeatureId::Auth => icons::KEY,
        FeatureId::Diagnostics => icons::CHART_BAR,
        FeatureId::Memory => icons::BRAIN,
        FeatureId::Improvement => icons::LIGHTBULB,
        FeatureId::Evaluation => icons::FLASK,
        FeatureId::PullRequests => icons::GIT_PULL_REQUEST,
        FeatureId::Maintenance => icons::WRENCH,
        FeatureId::Artifacts => icons::PACKAGE,
        FeatureId::Browser => icons::BROWSER,
        FeatureId::Restart => icons::ARROWS_CLOCKWISE,
    }
}

pub struct Viewer<'v, 'p> {
    pub pass: &'v mut DockPass<'p>,
    pub titles: &'v [(FeatureId, &'static str)],
    pub conversation: &'v mut ConversationPane,
    pub io: ConversationIo<'v>,
    pub selection: &'v Selection,
    pub effects: &'v mut Vec<Effect>,
    pub notifications: &'v mut NotificationLog,
    pub run_tabs: &'v RunTabs,
    pub local: &'v mut LocalPanes,
    pub attention: &'v mut TabAttention,
}

/// The current root run of the selected thread, as its detail names it.
pub fn selected_root(
    pass: &DockPass<'_>,
    selection: &Selection,
) -> Option<workbench_protocol::ids::RunId> {
    let thread = selection.thread.as_ref()?;
    pass.workspace()?
        .model()
        .details
        .get(thread.as_str())?
        .root_run
        .clone()
}

impl Viewer<'_, '_> {
    fn title_of(&self, feature: FeatureId) -> &'static str {
        self.titles
            .iter()
            .find(|(id, _)| *id == feature)
            .map_or(feature.namespace(), |(_, title)| title)
    }

    fn label(&self, tab: CoreTab) -> (&'static str, Cow<'_, str>) {
        let (icon, title) = match tab {
            CoreTab::Projects => (icons::FOLDER_SIMPLE, "Projects"),
            CoreTab::Conversation => (icons::CHAT_CIRCLE_TEXT, "Conversation"),
            CoreTab::Notifications => (icons::BELL, "Notifications"),
            CoreTab::Plan => (icons::LIST_CHECKS, "Plan"),
            CoreTab::Diff => (icons::GIT_DIFF, "Diff"),
            CoreTab::Terminal => (icons::TERMINAL_WINDOW, "Terminal"),
            // The embedded workbench counts finished subagents too.
            CoreTab::Feature(FeatureId::Execution) => {
                let thread = self.selection.thread.as_ref();
                let count = self
                    .io
                    .cache
                    .runs
                    .values()
                    .filter(|run| Some(&run.thread) == thread && run.parent_run.is_some())
                    .count();
                let title = format!("{}({count})", self.title_of(FeatureId::Execution));
                return (feature_icon(FeatureId::Execution), Cow::Owned(title));
            }
            CoreTab::Feature(feature) => (feature_icon(feature), self.title_of(feature)),
            CoreTab::Run(id) => {
                let title = self.run_tabs.get(id).map(|tab| tab.title.as_str());
                return (icons::SCROLL, Cow::Borrowed(title.unwrap_or("Transcript")));
            }
            CoreTab::File(id) => {
                let title = self.local.file_title(id).unwrap_or("File");
                return (icons::FILE_TEXT, Cow::Borrowed(title));
            }
        };
        (icon, Cow::Borrowed(title))
    }

    fn selected_root(&self) -> Option<workbench_protocol::ids::RunId> {
        selected_root(self.pass, self.selection)
    }

    fn attention(&self, tab: CoreTab) -> PaneAttention {
        match tab {
            CoreTab::Notifications if self.notifications.unread_count() > 0 => PaneAttention::Info,
            _ => self.attention.of(tab),
        }
    }

    fn notifications(&mut self, ui: &mut Ui) {
        let focused = ui.input(|input| input.viewport().focused == Some(true));
        let Some((project, thread)) = notifications::show(ui, self.notifications, focused) else {
            return;
        };
        let project = project.or_else(|| {
            self.pass
                .view()
                .cache
                .threads
                .get(thread.as_str())
                .map(|summary| summary.project.clone())
        });
        if let Some(project) = project {
            self.pass
                .navigate(Navigation::SelectThread { project, thread });
            self.effects.push(Effect::Open(CoreTab::Conversation));
        }
    }

    /// The workspace feature draws the whole tab, its own scroll area included; the
    /// pass maintains it like a feature whose tab is hidden.
    fn projects(&mut self, ui: &mut Ui) {
        ui.add_enabled_ui(self.io.connected, |ui| {
            if !self.pass.render_selector(ui) {
                builtin_selector(ui, self.pass, self.selection);
            }
        });
    }

    fn unavailable(&self, ui: &mut Ui, title: &str) {
        let hint = if !self.io.connected {
            "Connect to the core to use this area."
        } else {
            "The connected core does not offer this area."
        };
        empty_state(ui, title, hint, None);
    }

    /// The plan of the thread the execution feature follows. Selecting a task
    /// or paging the history is queued into the feature.
    fn plan(&mut self, ui: &mut Ui) {
        let Some(execution) = self.pass.execution() else {
            self.unavailable(ui, "Plan");
            return;
        };
        let action = plan_panel(ui, execution.plan_state());
        let Some(execution) = self.pass.execution_mut() else {
            return;
        };
        // Reads for a tab shown again leave in the next pass.
        let mut repaint = execution.plan_drawn();
        if let Some(action) = action {
            execution.enqueue_plan(action);
            repaint = true;
        }
        if repaint {
            ui.ctx().request_repaint();
        }
    }
}

impl TabViewer for Viewer<'_, '_> {
    type Tab = CoreTab;

    fn id(&mut self, tab: &mut CoreTab) -> egui::Id {
        egui::Id::new(("core-tab", *tab))
    }

    fn title(&mut self, tab: &mut CoreTab) -> egui::WidgetText {
        let (icon, title) = self.label(*tab);
        let title = icons::with_icon(icon, &*title);
        // egui_dock paints the title without a widget, so the prefix never reaches the
        // tab's accessible name, which `on_tab_button` sets from the plain label.
        tab_attention::tab_title(title, self.attention(*tab))
    }

    fn tab_style_override(
        &self,
        tab: &CoreTab,
        global_style: &egui_dock::TabStyle,
    ) -> Option<egui_dock::TabStyle> {
        tab_attention::tab_style(global_style, self.attention(*tab))
    }

    fn on_tab_button(&mut self, tab: &mut CoreTab, response: &egui::Response) {
        // egui_dock paints the title without a widget, so the tab needs its own name.
        let name = format!("{} tab", self.label(*tab).1);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut CoreTab) {
        let start = ui.ctx().output(|output| output.commands.len());
        let displayed = self.attention.displayed(*tab);
        let visible = ui.is_visible() && ui.clip_rect().intersects(ui.max_rect());
        match *tab {
            CoreTab::Projects => self.projects(ui),
            CoreTab::Notifications => self.notifications(ui),
            CoreTab::Conversation => {
                let workspace = self.pass.workspace();
                let thread = self.selection.thread.as_ref();
                self.conversation.sync_model_picker(
                    thread,
                    workspace.and_then(|w| {
                        self.selection
                            .project
                            .as_ref()
                            .and_then(|p| w.model_catalog(p.as_str()))
                    }),
                    workspace.and_then(|w| thread.and_then(|t| w.model().details.get(t.as_str()))),
                    workspace.is_some_and(|w| {
                        thread.is_some_and(|t| w.model_choice_pending(t.as_str()))
                    }),
                );
                let root = self.selected_root();
                let band = self.pass.execution().and_then(|e| e.band());
                let lane = self.pass.lanes().and_then(|lanes| lanes.card());
                let pull_request = self.pass.pull_requests().and_then(|prs| prs.bar());
                let chips = self.pass.pull_requests().and_then(|prs| prs.chips());
                self.conversation.set_mention_scope(self.local.roots());
                conversation::show(
                    ui,
                    self.conversation,
                    &mut self.io,
                    self.selection,
                    conversation::Bands {
                        band,
                        lane,
                        pull_request,
                        chips,
                    },
                    root.as_ref(),
                    self.effects,
                );
                if let Some(navigation) = self.conversation.take_navigation() {
                    self.pass.navigate(navigation);
                }
            }
            CoreTab::Plan => self.plan(ui),
            CoreTab::Diff => self.local.diff_tab(ui),
            CoreTab::Terminal => self.local.terminal_tab(ui),
            CoreTab::File(id) => self.local.file_tab(ui, id),
            CoreTab::Run(id) => {
                let phase = self.attention.run_phase(id);
                let chips = self.pass.pull_requests().and_then(|prs| prs.chips());
                conversation::run_tab(
                    ui,
                    self.conversation,
                    self.io.cache,
                    self.io.connected,
                    self.io.link_base,
                    conversation::RunTabView {
                        tab: self.run_tabs.get(id),
                        phase,
                        chips,
                    },
                    self.effects,
                );
            }
            CoreTab::Feature(feature) => {
                if !self.pass.render_tab(feature, ui) {
                    self.unavailable(ui, self.title_of(feature));
                } else if feature == FeatureId::Execution
                    && let Some(pane) = self
                        .pass
                        .execution_mut()
                        .and_then(|execution| execution.take_open_pane())
                {
                    self.effects.push(Effect::OpenRun(pane));
                }
            }
        }
        if visible
            && self
                .attention
                .acknowledge(displayed, ui.input(|input| input.viewport().focused))
        {
            ui.ctx().request_repaint();
        }
        let base = match *tab {
            CoreTab::File(id) => self.local.file_base(id),
            _ => self.io.link_base,
        };
        let links = local::take_links(ui.ctx(), start, base);
        if !links.is_empty() {
            ui.ctx().request_repaint();
        }
        for link in links {
            let file = self.local.open_file(ui.ctx(), link);
            self.effects.push(Effect::Open(CoreTab::File(file)));
        }
    }

    fn is_closeable(&self, tab: &CoreTab) -> bool {
        matches!(
            tab,
            CoreTab::Feature(_)
                | CoreTab::Plan
                | CoreTab::Run(_)
                | CoreTab::Diff
                | CoreTab::Terminal
                | CoreTab::File(_)
        )
    }

    // A tab that cannot be closed stays on the main surface, so a saved layout always
    // has a main surface to restore.
    fn allowed_in_windows(&self, tab: &mut CoreTab) -> bool {
        self.is_closeable(tab)
    }

    fn scroll_bars(&self, _tab: &CoreTab) -> [bool; 2] {
        [false, false]
    }
}

/// The projects and threads of the snapshot, drawn while the workspace feature is not
/// available (the tab is disabled then).
fn builtin_selector(ui: &mut Ui, pass: &mut DockPass<'_>, selection: &Selection) {
    let view = pass.view();
    let mut navigations = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("core-projects")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for project in view.cache.projects.values() {
                let id = project.id.as_str();
                let folded = egui::Id::new(("core-builtin-folded", id));
                let open = !ui.data(|data| data.get_temp::<bool>(folded).unwrap_or(false));
                let threads: Vec<_> = view
                    .cache
                    .threads
                    .values()
                    .filter(|t| t.project == project.id)
                    .collect();
                let clicks = ui
                    .push_id(id, |ui| {
                        project_row(
                            ui,
                            ProjectRowView {
                                name: &project.display_name,
                                hover_root: None,
                                selected: selection.project.as_ref() == Some(&project.id),
                                open,
                                count: threads.len(),
                                search: false,
                                settings: false,
                            },
                        )
                    })
                    .inner;
                if clicks.toggle {
                    ui.data_mut(|data| data.insert_temp(folded, open));
                }
                if clicks.select {
                    navigations.push(Navigation::SelectProject(project.id.clone()));
                }
                if clicks.new_thread {
                    navigations.push(Navigation::NewConversation(project.id.clone()));
                }
                if !open {
                    continue;
                }
                for thread in threads {
                    let selected = selection.thread.as_ref() == Some(&thread.id);
                    let clicked = ui
                        .push_id(thread.id.as_str(), |ui| {
                            let mut clicked = false;
                            compact_row(ui, selected, |ui| {
                                ui.add_space(SP_2 + INDENT);
                                clicked = row_title(ui, thread.title.as_str()).clicked();
                            });
                            clicked
                        })
                        .inner;
                    if clicked {
                        navigations.push(Navigation::SelectThread {
                            project: project.id.clone(),
                            thread: thread.id.clone(),
                        });
                    }
                }
            }
        });
    for navigation in navigations {
        pass.navigate(navigation);
    }
}

pub struct FooterState<'a> {
    pub quotas: &'a [workbench_protocol::namespaces::diagnostics::ProviderQuota],
    pub stats: Option<&'a crate::panes::system_footer::stats::SystemSnapshot>,
    pub today_cost: Option<String>,
    pub status: &'a str,
    pub connected: bool,
    pub outcome_unknown: &'a BTreeSet<String>,
    pub unsupported: &'a [(&'static str, &'static str)],
    pub titles: &'a [(FeatureId, &'static str)],
    pub placeholders: &'a [FeatureId],
}

pub fn footer(ui: &mut Ui, state: FooterState<'_>, effects: &mut Vec<Effect>) {
    crate::panes::system_footer::with_right_stats(ui, state.stats, |ui| {
        footer_content(ui, state, effects);
    });
}

/// One entry of the ⚙ menu: an icon before its name, which is also its accessible name and its
/// tooltip, and an optional hint at the right.
fn menu_item(ui: &mut Ui, icon: &str, name: &str, hint: Option<&str>) -> bool {
    let mut button = egui::Button::new(icons::with_icon(icon, name));
    if let Some(hint) = hint {
        button = button.right_text(muted(hint).size(FONT_SMALL));
    }
    let response = ui.add(button);
    accessible(&response, name);
    let clicked = response.clicked();
    response.on_hover_text(hint.map_or_else(|| name.to_owned(), |hint| format!("{name} ({hint})")));
    if clicked {
        ui.close();
    }
    clicked
}

fn menu_heading(ui: &mut Ui, title: &str) {
    ui.label(section(title.to_uppercase()));
}

/// The ⚙ menu: Configure, Core and View, each entry with its icon. Model Settings is the one way
/// into Providers, Routing and Agent Roles.
fn settings_menu(ui: &mut Ui, state: &FooterState<'_>, effects: &mut Vec<Effect>) {
    let shown = |feature: FeatureId| !state.placeholders.contains(&feature);
    let open = |effects: &mut Vec<Effect>, feature: FeatureId| {
        effects.push(Effect::Open(CoreTab::Feature(feature)));
    };
    menu_heading(ui, "Configure");
    if shown(FeatureId::Settings)
        && menu_item(
            ui,
            icons::STACK_SIMPLE,
            "Model Settings",
            Some("Providers \u{b7} Routing \u{b7} Agent Roles"),
        )
    {
        effects.push(Effect::Settings(SettingsIntent::ModelSettings(
            crate::panes::settings::SettingsTab::Providers,
        )));
    }
    // Until Providers takes sign-in over, credentials are managed in their own tab.
    if shown(FeatureId::Auth)
        && menu_item(ui, feature_icon(FeatureId::Auth), "Authentication", None)
    {
        open(effects, FeatureId::Auth);
    }
    if shown(FeatureId::Mcp) && menu_item(ui, feature_icon(FeatureId::Mcp), "MCP servers", None) {
        open(effects, FeatureId::Mcp);
    }
    if menu_item(ui, icons::PALETTE, "Appearance", None) {
        effects.push(Effect::ThemeSettings);
    }
    ui.separator();
    menu_heading(ui, "Core");
    if shown(FeatureId::Settings)
        && menu_item(
            ui,
            feature_icon(FeatureId::Settings),
            "Config file",
            Some("config.toml"),
        )
    {
        effects.push(Effect::Settings(SettingsIntent::ConfigFile));
    }
    if shown(FeatureId::Restart)
        && menu_item(ui, feature_icon(FeatureId::Restart), "Restart core", None)
    {
        open(effects, FeatureId::Restart);
    }
    if shown(FeatureId::Maintenance)
        && menu_item(
            ui,
            feature_icon(FeatureId::Maintenance),
            "Maintenance",
            None,
        )
    {
        open(effects, FeatureId::Maintenance);
    }
    ui.separator();
    menu_heading(ui, "View");
    if shown(FeatureId::Diagnostics)
        && menu_item(ui, feature_icon(FeatureId::Diagnostics), "Usage", None)
    {
        open(effects, FeatureId::Diagnostics);
    }
    if shown(FeatureId::Evaluation)
        && menu_item(ui, feature_icon(FeatureId::Evaluation), "Evaluations", None)
    {
        open(effects, FeatureId::Evaluation);
    }
    panels_menu(ui, state, effects);
    if menu_item(ui, icons::ARROW_COUNTER_CLOCKWISE, "Reset layout", None) {
        effects.push(Effect::ResetLayout);
    }
}

fn panels_menu(ui: &mut Ui, state: &FooterState<'_>, effects: &mut Vec<Effect>) {
    let response = ui.menu_button(icons::with_icon(icons::LAYOUT, "Panels"), |ui| {
        for tab in [
            CoreTab::Projects,
            CoreTab::Conversation,
            CoreTab::Notifications,
            CoreTab::Plan,
            CoreTab::Diff,
            CoreTab::Terminal,
        ] {
            let label = match tab {
                CoreTab::Projects => "Projects",
                CoreTab::Notifications => "Notifications",
                CoreTab::Plan => "Plan",
                CoreTab::Diff => "Diff",
                CoreTab::Terminal => "Terminal",
                _ => "Conversation",
            };
            if ui.button(label).clicked() {
                effects.push(Effect::Open(tab));
                ui.close();
            }
        }
        for (feature, name) in state.titles {
            // Config file and Authentication have their own entries in the menu above.
            if matches!(
                feature,
                FeatureId::Workspace | FeatureId::Settings | FeatureId::Auth
            ) || state.placeholders.contains(feature)
            {
                continue;
            }
            if ui
                .button(icons::with_icon(feature_icon(*feature), *name))
                .clicked()
            {
                effects.push(Effect::Open(CoreTab::Feature(*feature)));
                ui.close();
            }
        }
    });
    accessible(&response.response, "Panels");
}

fn footer_content(ui: &mut Ui, state: FooterState<'_>, effects: &mut Vec<Effect>) {
    ui.horizontal_wrapped(|ui| {
        ui.menu_button("⚙", |ui| {
            settings_menu(ui, &state, effects);
        })
        .response
        .on_hover_text("Workbench settings");
        let usage = crate::panes::system_footer::usage_label(state.today_cost.clone());
        if ghost_icon_button(ui, icons::CHART_BAR, &usage)
            .on_hover_text("Token usage statistics")
            .clicked()
        {
            effects.push(Effect::Open(CoreTab::Feature(FeatureId::Diagnostics)));
        }
        if ghost_icon_button(ui, icons::FLASK, "Evaluations")
            .on_hover_text("Evaluations the Evaluator started")
            .clicked()
        {
            effects.push(Effect::Open(CoreTab::Feature(FeatureId::Evaluation)));
        }
        super::features::diagnostics::quota::footer(ui, state.quotas);
        ui.separator();
        let color = if state.connected {
            palette().SUCCESS
        } else {
            palette().WARNING
        };
        status_dot(ui, color).on_hover_text(if state.connected {
            "Connected to the core"
        } else {
            "Not connected. Nothing runs in this window in the core's place."
        });
        ui.label(muted(state.status).size(FONT_SMALL));
        if ghost_icon_button(ui, icons::ARROWS_CLOCKWISE, "Reconnect")
            .on_hover_text("Reconnect to the core and read its state again")
            .clicked()
        {
            effects.push(Effect::send(Action::Reconnect));
        }
        if !state.outcome_unknown.is_empty() {
            let text = format!(
                "{} outcome{} unknown",
                state.outcome_unknown.len(),
                if state.outcome_unknown.len() == 1 { "" } else { "s" }
            );
            let mut hover = String::from(
                "These requests were sent, but their replies were lost. They are not sent again.",
            );
            for request in state.outcome_unknown {
                hover.push('\n');
                hover.push_str(request);
            }
            ui.label(
                RichText::new(icons::with_icon(icons::WARNING, text))
                    .size(FONT_SMALL)
                    .color(palette().WARNING_FG),
            )
            .on_hover_text(hover);
        }
        if !state.unsupported.is_empty() {
            ui.menu_button(
                muted(icons::with_icon(icons::INFO, "Unsupported")).size(FONT_SMALL),
                |ui| {
                    ui.label(
                        "Only the operations shown are available here. The database is never opened directly to make up for the rest.",
                    );
                    for (title, note) in state.unsupported {
                        ui.label(format!("{title}: {note}"));
                    }
                },
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remove(dock: &mut DockState<CoreTab>, tab: CoreTab) {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        dock.remove_tab(path);
    }

    fn leaf_of(dock: &DockState<CoreTab>, tab: CoreTab) -> Vec<CoreTab> {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        dock.leaf(path.node_path()).unwrap().tabs.clone()
    }

    fn front(dock: &DockState<CoreTab>, tab: CoreTab) -> CoreTab {
        let path = dock.find_tab(&tab).expect("the tab is in the dock");
        let leaf = dock.leaf(path.node_path()).unwrap();
        leaf.tabs[leaf.active.0]
    }

    #[test]
    fn the_default_dock_has_the_default_leaves() {
        let dock = default_dock();
        let mut leaves: Vec<_> = dock
            .iter_leaves()
            .map(|(_, leaf)| leaf.tabs.clone())
            .collect();
        let mut expected = default_leaves().to_vec();
        leaves.sort_by_key(|tabs| format!("{tabs:?}"));
        expected.sort_by_key(|tabs| format!("{tabs:?}"));
        assert_eq!(leaves, expected);
    }

    // Given: the default dock with Lanes closed
    // When: Lanes is focused again
    // Then: it is back in the notifications' leaf between Diff and Memory, in front
    #[test]
    fn a_closed_default_tab_reopens_in_its_leaf() {
        let mut dock = default_dock();
        let lanes = CoreTab::Feature(FeatureId::Lanes);
        remove(&mut dock, lanes);

        focus(&mut dock, lanes);

        assert_eq!(
            leaf_of(&dock, lanes),
            [
                CoreTab::Notifications,
                CoreTab::Diff,
                lanes,
                CoreTab::Feature(FeatureId::Memory),
                CoreTab::Feature(FeatureId::Improvement),
            ]
        );
        assert_eq!(front(&dock, lanes), lanes);
    }

    // Given: Teams closed, and then Execution too
    // When: Teams is focused each time
    // Then: it goes back beside Execution; without Execution, to the conversation's leaf
    #[test]
    fn a_tab_whose_leaf_is_gone_opens_beside_the_conversation() {
        let teams = CoreTab::Feature(FeatureId::Teams);
        let execution = CoreTab::Feature(FeatureId::Execution);
        let mut dock = default_dock();
        remove(&mut dock, teams);
        focus(&mut dock, teams);
        assert_eq!(leaf_of(&dock, teams), [execution, teams]);

        remove(&mut dock, teams);
        remove(&mut dock, execution);
        focus(&mut dock, teams);
        assert_eq!(
            leaf_of(&dock, teams),
            [
                CoreTab::Conversation,
                CoreTab::Feature(FeatureId::Browser),
                teams
            ]
        );
    }

    // Given: the default dock
    // When: Settings, which has no default leaf, is focused
    // Then: it opens at the end of the conversation's leaf
    #[test]
    fn an_on_demand_tab_opens_beside_the_conversation() {
        let settings = CoreTab::Feature(FeatureId::Settings);
        let mut dock = default_dock();

        focus(&mut dock, settings);

        assert_eq!(
            leaf_of(&dock, settings),
            [
                CoreTab::Conversation,
                CoreTab::Feature(FeatureId::Browser),
                settings
            ]
        );
    }
}
