use runtime::{AgentInspection, ProjectTrust};
use workspace_ui::{Membership, SidebarState, TrustState};

pub const fn to_project_trust(trust: TrustState) -> ProjectTrust {
    match trust {
        TrustState::Approved => ProjectTrust::Approved,
        TrustState::Unapproved => ProjectTrust::Unapproved,
    }
}

pub fn run_membership(sidebar: &SidebarState, inspection: &AgentInspection) -> Membership {
    let inspected_path = inspection
        .workspace
        .as_ref()
        .and_then(|workspace| workspace.worktree_path.as_deref());

    if let Some(path) = inspected_path {
        return sidebar
            .projects
            .iter()
            .map(|project| project.resolve_membership(path))
            .find(|membership| !matches!(membership, Membership::Outside))
            .unwrap_or(Membership::Outside);
    }

    sidebar
        .selected_project
        .as_ref()
        .and_then(|selected| {
            sidebar
                .projects
                .iter()
                .find(|project| &project.id == selected)
        })
        .or_else(|| sidebar.projects.first())
        .map_or(Membership::Outside, |project| {
            project.resolve_membership(&project.repo_root)
        })
}

#[cfg(test)]
mod tests {
    use event_bus::AgentRunPhase;
    use runtime::{MergeMode, RunId, WorkspaceInspection, WorkspaceMode};
    use workspace_ui::ProjectId;

    use super::*;

    fn inspection(worktree_path: Option<std::path::PathBuf>) -> AgentInspection {
        AgentInspection {
            run_id: RunId::new(1),
            role_name: "Worker".into(),
            phase: AgentRunPhase::Running,
            message_count: 0,
            workspace: Some(WorkspaceInspection {
                mode: WorkspaceMode::Isolated,
                branch: Some("evorch/task/run-1".into()),
                worktree_path,
                merge_mode: MergeMode::Branch,
            }),
        }
    }

    #[test]
    fn trust_conversion_preserves_approval() {
        // Given: both workspace trust variants
        // When: they are converted for runtime rules
        // Then: approval and rejection are preserved exactly
        assert_eq!(
            to_project_trust(TrustState::Approved),
            ProjectTrust::Approved
        );
        assert_eq!(
            to_project_trust(TrustState::Unapproved),
            ProjectTrust::Unapproved
        );
    }

    #[test]
    fn missing_worktree_falls_back_to_selected_project_root() {
        // Given: a selected project and an inspection without a worktree path
        let temp = tempfile::tempdir().expect("temp dir");
        let mut sidebar = SidebarState::default();
        let project_id = ProjectId::new("demo");
        sidebar
            .add_project(project_id.clone(), "demo", temp.path())
            .expect("project can be added");
        sidebar
            .select_project(&project_id)
            .expect("project can be selected");

        // When: membership is resolved
        let membership = run_membership(&sidebar, &inspection(None));

        // Then: the run inherits project-root membership
        assert_eq!(membership, Membership::ProjectRoot);
    }
}
