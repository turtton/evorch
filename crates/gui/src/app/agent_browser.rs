//! The agent browser tab beside the active conversation.
//!
//! Each conversation that has an agent page gets the tab in the subagent
//! region. It shows the run that last used the browser tools, a page that
//! closed keeps its last frame, and closing the tab hides it until the
//! conversation opens another page or the user picks View on a browser card.

use std::collections::{BTreeMap, BTreeSet};

use egui_dock::{NodeIndex, NodePath, SurfaceIndex, TabIndex};
use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;
use crate::panes::agent_browser::{
    AgentBrowserSurface, BrowserRun, BrowserTabView, is_browser_tool, set_available,
    take_open_request,
};

pub(super) const PANEL: &str = "agent-browser-main";

#[derive(Default)]
pub(super) struct AgentBrowserTabs {
    surface: Option<Box<dyn AgentBrowserSurface>>,
    /// Runs with an open page as of the last frame.
    open_pages: BTreeSet<String>,
    /// Per conversation thread.
    threads: BTreeMap<String, ThreadBrowser>,
    /// Whether the tab drew this frame; a hidden tab stops its screencast.
    rendered: bool,
    /// The user closed the tab this frame.
    closed: bool,
    /// The thread the tab in the dock belongs to.
    owner: Option<String>,
}

#[derive(Default)]
struct ThreadBrowser {
    /// Runs that opened a page, oldest first.
    runs: Vec<String>,
    selected: Option<String>,
    /// Whether the thread shows the tab.
    shown: bool,
    /// Whether the tab was in front of its leaf when the thread was left.
    front: bool,
}

impl ThreadBrowser {
    fn show(&mut self, run: &str) {
        if !self.runs.iter().any(|known| known == run) {
            self.runs.push(run.to_owned());
        }
        self.selected = Some(run.to_owned());
        self.shown = true;
    }
}

impl AgentBrowserTabs {
    /// Draws the tab for `thread`. `label` names a run.
    pub(super) fn render(
        &mut self,
        ui: &mut egui::Ui,
        thread: Option<&str>,
        label: impl Fn(&str) -> String,
    ) {
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        self.rendered = true;
        let state = thread.and_then(|thread| self.threads.get_mut(thread));
        let runs: Vec<BrowserRun> = state
            .as_ref()
            .map(|state| {
                state
                    .runs
                    .iter()
                    .map(|run| BrowserRun {
                        run_id: run.clone(),
                        label: label(run),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let view = BrowserTabView {
            runs: &runs,
            selected: state.as_ref().and_then(|state| state.selected.as_deref()),
        };
        if let Some(picked) = surface.render(ui, view)
            && let Some(state) = state
        {
            state.selected = Some(picked);
        }
    }

    pub(super) fn closed_by_user(&mut self) {
        self.closed = true;
    }
}

impl<S: AgentRunSource> WorkbenchState<S> {
    /// Shows agent pages in the agent browser tab. Without it, the tab and
    /// the View buttons on browser tool cards stay hidden.
    pub fn set_agent_browser(&mut self, surface: Box<dyn AgentBrowserSurface>) {
        self.agent_browser.surface = Some(surface);
    }

    /// A browser tool call moves the tab to its run once that run has a page.
    pub(super) fn fold_agent_browser_event(&mut self, event: &event_bus::Event) {
        let event_bus::EventKind::Tool(event_bus::ToolEvent::ToolStarted {
            tool_name,
            run_id: Some(run_id),
            ..
        }) = &event.kind
        else {
            return;
        };
        if !is_browser_tool(tool_name) {
            return;
        }
        let Some(thread) = self.thread_for_run(run_id) else {
            return;
        };
        if let Some(state) = self.agent_browser.threads.get_mut(&thread)
            && state.runs.iter().any(|run| run == run_id)
        {
            state.selected = Some(run_id.clone());
        }
    }

    /// Before the dock draws: follow newly opened pages and View requests,
    /// and keep the tab only beside a conversation that shows it.
    pub(super) fn sync_agent_browser(&mut self, ctx: &egui::Context) {
        self.agent_browser.rendered = false;
        let id = PanelId::new(PANEL);
        let Some(surface) = self.agent_browser.surface.as_mut() else {
            set_available(ctx, false);
            self.remove_agent_browser_tab();
            return;
        };
        set_available(ctx, true);
        let pages: BTreeSet<String> = surface.pages(ctx).into_iter().collect();
        let opened: Vec<String> = pages
            .difference(&self.agent_browser.open_pages)
            .cloned()
            .collect();
        self.agent_browser.open_pages = pages;
        let active = self.sidebar.active_thread.as_ref().map(ToString::to_string);
        let mut raise = false;
        for run in opened {
            if let Some(thread) = self.thread_for_run(&run) {
                raise |= active.as_ref() == Some(&thread);
                self.agent_browser
                    .threads
                    .entry(thread)
                    .or_default()
                    .show(&run);
            }
        }
        let mut reveal = false;
        if let Some(run) = take_open_request(ctx)
            && let Some(thread) = self.thread_for_run(&run)
        {
            reveal = active.as_ref() == Some(&thread);
            self.agent_browser
                .threads
                .entry(thread)
                .or_default()
                .show(&run);
        }
        let shown = active
            .as_ref()
            .and_then(|thread| self.agent_browser.threads.get(thread))
            .is_some_and(|state| state.shown);
        if !shown {
            self.hide_agent_browser_tab();
            return;
        }
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind: PanelKind::AgentBrowser,
            title: PanelKind::AgentBrowser.default_title().into(),
            target: None,
        });
        if self.dock.find_tab(&id).is_none() {
            let front = active
                .as_ref()
                .and_then(|thread| self.agent_browser.threads.get(thread))
                .is_some_and(|state| state.front);
            self.place_agent_browser_tab(raise || reveal || front);
        } else if raise {
            self.raise_agent_browser_tab();
        }
        self.agent_browser.owner = active;
        if reveal {
            self.focus_panel(PANEL);
        }
    }

    /// After the dock drew: remember a close, and stop watching a tab that
    /// was not drawn (another tab of its leaf is in front, or it is gone).
    pub(super) fn finish_agent_browser_frame(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.agent_browser.closed)
            && let Some(thread) = self.sidebar.active_thread.as_ref()
            && let Some(state) = self.agent_browser.threads.get_mut(&thread.to_string())
        {
            state.shown = false;
        }
        if !self.agent_browser.rendered
            && let Some(surface) = self.agent_browser.surface.as_mut()
        {
            surface.stop(ctx);
        }
    }

    /// Agent pages do not outlive the process, so a saved layout's tab is dropped.
    pub(super) fn drop_saved_agent_browser(&mut self) {
        self.remove_agent_browser_tab();
        self.panels
            .retain(|_, panel| panel.kind != PanelKind::AgentBrowser);
    }

    /// Takes the tab out of the dock, remembering for its thread whether it
    /// was in front so that coming back shows it the same way.
    fn hide_agent_browser_tab(&mut self) {
        let front = self
            .dock
            .find_tab(&PanelId::new(PANEL))
            .and_then(|path| Some(self.dock.leaf(path.node_path()).ok()?.active == path.tab));
        if let Some(front) = front
            && let Some(owner) = self.agent_browser.owner.take()
            && let Some(state) = self.agent_browser.threads.get_mut(&owner)
        {
            state.front = front;
        }
        self.remove_agent_browser_tab();
    }

    fn remove_agent_browser_tab(&mut self) {
        while let Some(path) = self.dock.find_tab(&PanelId::new(PANEL)) {
            self.dock.remove_tab(path);
        }
    }

    /// Adds the tab to the subagent region without taking keyboard focus.
    fn place_agent_browser_tab(&mut self, raise: bool) {
        let id = PanelId::new(PANEL);
        let focus = self.subagent_focus_snapshot();
        let region = self
            .dock
            .find_tab(&PanelId::new("subagents-home"))
            .filter(|path| path.surface == SurfaceIndex::main())
            .map(|path| path.node)
            .or_else(|| self.subagent_leaves().first().copied());
        let leaf = region.and_then(|node| {
            self.dock
                .leaf_mut(NodePath {
                    surface: SurfaceIndex::main(),
                    node,
                })
                .ok()
        });
        match leaf {
            Some(leaf) => {
                leaf.tabs.push(id);
                if raise {
                    leaf.active = TabIndex(leaf.tabs.len() - 1);
                }
            }
            None => {
                let tree = self.dock.main_surface_mut();
                if tree.is_empty() {
                    *tree = egui_dock::Tree::new(vec![id]);
                } else {
                    tree.split_right(NodeIndex::root(), 0.7, vec![id]);
                }
            }
        }
        self.restore_subagent_focus(focus);
    }

    fn raise_agent_browser_tab(&mut self) {
        if let Some(path) = self.dock.find_tab(&PanelId::new(PANEL))
            && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
        {
            leaf.active = path.tab;
        }
    }
}

#[cfg(test)]
#[path = "agent_browser_tests.rs"]
mod tests;
