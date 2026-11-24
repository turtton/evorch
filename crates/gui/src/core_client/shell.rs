//! The window frame of the core client, laid out as the embedded workbench: a dock with
//! the projects on the left, the conversation in the middle and the areas of the core in
//! tabs around it, over a footer with the menus.
use std::borrow::Cow;
use std::collections::BTreeSet;

use egui::{RichText, Ui};
use egui_dock::{DockState, NodeIndex, TabIndex, TabViewer};
use workbench_protocol::ids::{ProjectId, ThreadId};

use super::connection::Action;
use super::conversation::{self, ConversationIo, ConversationPane};
use super::features::{DockPass, FeatureId, Navigation, Selection};
use super::local::LocalPanes;
use super::notifications::{self, NotificationLog};
use super::run_tabs::{RunPane, RunTabId, RunTabs};
use crate::panes::conversation_plan::plan_panel;
use crate::panes::goal_widgets::GoalAction;
use crate::panes::sidebar_widgets::{INDENT, ProjectRowView, project_row};
use crate::theme::icons;
use crate::theme::text::muted;
use crate::theme::tokens::*;
use crate::theme::widgets::{compact_row, empty_state, ghost_icon_button, row_title, status_dot};

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
}

/// What a frame asks the screen to do once the dock is drawn and the shared state is free.
pub enum Effect {
    Send(Box<Action>),
    Reveal(FeatureId),
    Open(CoreTab),
    /// Open pane pressed on an Execution row.
    OpenRun(RunPane),
    /// A goal control pressed in the composer band, for the execution feature.
    Goal(GoalAction),
    ResetLayout,
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
    DiscardAttachments(ThreadId),
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

pub const ON_DEMAND: [FeatureId; 7] = [
    FeatureId::Settings,
    FeatureId::Auth,
    FeatureId::Mcp,
    FeatureId::Diagnostics,
    FeatureId::Evaluation,
    FeatureId::Restart,
    FeatureId::Maintenance,
];

const SIDEBAR_FRACTION: f32 = 0.30;
/// The conversation's share of the centre column above the Terminal, as in the
/// embedded workbench.
const TERMINAL_FRACTION: f32 = 0.7;

pub fn default_dock() -> DockState<CoreTab> {
    let mut centre = vec![CoreTab::Conversation];
    centre.extend(CENTRE.map(CoreTab::Feature));
    let mut dock = DockState::new(centre);
    let surface = dock.main_surface_mut();
    let [middle, left] =
        surface.split_left(NodeIndex::root(), SIDEBAR_FRACTION, vec![CoreTab::Projects]);
    let mut bottom_left = vec![CoreTab::Notifications, CoreTab::Diff];
    bottom_left.extend(LEFT.map(CoreTab::Feature));
    surface.split_below(left, 0.65, bottom_left);
    let [conversation, right] =
        surface.split_right(middle, 0.7, RIGHT_TOP.map(CoreTab::Feature).to_vec());
    surface.split_below(right, 0.5, RIGHT_BOTTOM.map(CoreTab::Feature).to_vec());
    let [_, terminal] =
        surface.split_below(conversation, TERMINAL_FRACTION, vec![CoreTab::Terminal]);
    // Folded to its tab bar until opened, as in the embedded workbench: no shell starts
    // before the person asks for one, and the conversation keeps its height.
    if let Ok(leaf) = surface.leaf_mut(terminal) {
        leaf.collapsed = true;
    }
    dock
}

pub fn focus(dock: &mut DockState<CoreTab>, tab: CoreTab) {
    if dock.find_tab(&tab).is_none() {
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
    if dock.find_tab(&tab).is_some() {
        return;
    }
    let (path, index) = match (
        dock.find_tab(&anchor),
        dock.find_tab(&CoreTab::Conversation),
    ) {
        (Some(path), _) => (path, Some(path.tab.0 + 1)),
        (None, Some(path)) => (path, None),
        (None, None) => {
            dock.push_to_first_leaf(tab);
            return;
        }
    };
    if let Ok(leaf) = dock.leaf_mut(path.node_path()) {
        let index = index.map_or(leaf.tabs.len(), |index| index.min(leaf.tabs.len()));
        leaf.tabs.insert(index, tab);
        if leaf.active.0 >= index && leaf.tabs.len() > 1 {
            leaf.active = TabIndex(leaf.active.0 + 1);
        }
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
        FeatureId::Settings => icons::GEAR,
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
            CoreTab::Feature(feature) => (feature_icon(feature), self.title_of(feature)),
            CoreTab::Run(id) => {
                let title = self.run_tabs.get(id).map(|tab| tab.title.as_str());
                return (icons::SCROLL, Cow::Borrowed(title.unwrap_or("Transcript")));
            }
        };
        (icon, Cow::Borrowed(title))
    }

    /// The current root run of the selected thread, as its detail names it.
    fn selected_root(&self) -> Option<workbench_protocol::ids::RunId> {
        let thread = self.selection.thread.as_ref()?;
        let workspace = self.pass.workspace()?;
        workspace
            .model()
            .details
            .get(thread.as_str())?
            .root_run
            .clone()
    }

    fn unread(&self, tab: CoreTab) -> bool {
        tab == CoreTab::Notifications && self.notifications.unread_count() > 0
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
        if self.unread(*tab) {
            RichText::new(format!("• {title}"))
                .color(palette().INFO)
                .into()
        } else {
            title.into()
        }
    }

    fn tab_style_override(
        &self,
        tab: &CoreTab,
        global_style: &egui_dock::TabStyle,
    ) -> Option<egui_dock::TabStyle> {
        self.unread(*tab)
            .then(|| crate::theme::dock::attention_tab_style(global_style, palette().INFO))
    }

    fn on_tab_button(&mut self, tab: &mut CoreTab, response: &egui::Response) {
        // egui_dock paints the title without a widget, so the tab needs its own name.
        let name = format!("{} tab", self.label(*tab).1);
        response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, &name));
    }

    fn ui(&mut self, ui: &mut Ui, tab: &mut CoreTab) {
        match *tab {
            CoreTab::Projects => self.projects(ui),
            CoreTab::Notifications => self.notifications(ui),
            CoreTab::Conversation => {
                let root = self.selected_root();
                let band = self.pass.execution().and_then(|e| e.band());
                conversation::show(
                    ui,
                    self.conversation,
                    &mut self.io,
                    self.selection,
                    band,
                    root.as_ref(),
                    self.effects,
                );
            }
            CoreTab::Plan => self.plan(ui),
            CoreTab::Diff => self.local.diff_tab(ui),
            CoreTab::Terminal => self.local.terminal_tab(ui),
            CoreTab::Run(id) => conversation::run_tab(
                ui,
                self.conversation,
                self.io.cache,
                self.io.connected,
                self.run_tabs.get(id),
                self.effects,
            ),
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
    }

    fn is_closeable(&self, tab: &CoreTab) -> bool {
        matches!(
            tab,
            CoreTab::Feature(_)
                | CoreTab::Plan
                | CoreTab::Run(_)
                | CoreTab::Diff
                | CoreTab::Terminal
        )
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
    pub status: &'a str,
    pub connected: bool,
    pub outcome_unknown: &'a BTreeSet<String>,
    pub unsupported: &'a [(&'static str, &'static str)],
    pub titles: &'a [(FeatureId, &'static str)],
    pub placeholders: &'a [FeatureId],
}

pub fn footer(ui: &mut Ui, state: FooterState<'_>, effects: &mut Vec<Effect>) {
    let title = |feature: FeatureId| {
        state
            .titles
            .iter()
            .find(|(id, _)| *id == feature)
            .map_or(feature.namespace(), |(_, title)| *title)
    };
    ui.horizontal_wrapped(|ui| {
        ui.menu_button("⚙", |ui| {
            for feature in ON_DEMAND {
                if state.placeholders.contains(&feature) {
                    continue;
                }
                if ui
                    .button(icons::with_icon(feature_icon(feature), title(feature)))
                    .clicked()
                {
                    effects.push(Effect::Open(CoreTab::Feature(feature)));
                    ui.close();
                }
            }
            ui.separator();
            ui.menu_button("Panels", |ui| {
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
                    if *feature == FeatureId::Workspace || state.placeholders.contains(feature) {
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
            if ui.button("Reset layout").clicked() {
                effects.push(Effect::ResetLayout);
                ui.close();
            }
        })
        .response
        .on_hover_text("Workbench settings");
        if ghost_icon_button(ui, icons::CHART_BAR, "Usage")
            .on_hover_text("Token usage and run diagnostics")
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
