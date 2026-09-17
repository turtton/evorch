use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn register_durable_tasks_panel(&mut self) {
        let id = PanelId::new("durable-tasks-main");
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind: PanelKind::DurableTasks,
            title: PanelKind::DurableTasks.default_title().into(),
            target: None,
        });
        if self.dock.find_tab(&id).is_some() {
            return;
        }
        let target = self
            .dock
            .find_tab(&PanelId::new("agents-main"))
            .or_else(|| self.dock.find_tab(&PanelId::new("tasks-main")))
            .or_else(|| self.dock.iter_all_tabs().next().map(|(path, _)| path));
        if let Some(path) = target
            && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
        {
            leaf.tabs.push(id);
        }
    }
}
