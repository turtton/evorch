use workspace_ui::{Panel, PanelId, PanelKind};

use super::WorkbenchState;
use crate::model::tasks::AgentRunSource;

impl<S: AgentRunSource> WorkbenchState<S> {
    pub(super) fn register_work_panels(&mut self) {
        self.register_work_panel("tasks-main", PanelKind::Tasks, "agents-main");
        self.register_work_panel("agents-main", PanelKind::Agents, "tasks-main");
    }

    fn register_work_panel(&mut self, id: &str, kind: PanelKind, neighbor: &str) {
        let id = PanelId::new(id);
        self.panels.entry(id.clone()).or_insert_with(|| Panel {
            id: id.clone(),
            kind,
            title: kind.default_title().into(),
            target: None,
        });
        if self.dock.find_tab(&id).is_some() {
            return;
        }
        let target = self
            .dock
            .find_tab(&PanelId::new(neighbor))
            .or_else(|| self.dock.iter_all_tabs().next().map(|(path, _)| path));
        if let Some(path) = target
            && let Ok(leaf) = self.dock.leaf_mut(path.node_path())
        {
            leaf.tabs.push(id);
        }
    }
}
