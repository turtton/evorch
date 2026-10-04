//! Preview of the base context a new run would receive, computed without calling a model.

use std::path::Path;
use std::sync::Arc;

use agents::Role;
use providers::ToolSpec;

use super::{
    ContextExclusion, ContextSection, ContextSectionKind, ContextSource, ContextTool, SystemInputs,
    compose_system_sections, resolve_preset_origins, workspace_note,
};
use crate::agent_loop::{
    LoopShared, append_subagent_context_note, shared_active_root, standard_tool_specs,
    visible_tool_specs,
};
use crate::compaction::estimator::estimate_tool_tokens;
use crate::memory::MemoryBoundary;
use crate::prompt::{AvailableSkill, classify};
use crate::rules::{StartupRuleExclusion, startup_rule_files};
use crate::{AgentRuntime, ExecutionPolicy, RunConfig, RuntimeError, WorkspaceMode};

/// The run shape to preview.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseContextRequest {
    pub role: Role,
    pub category: Option<String>,
    pub load_skills: Vec<String>,
    /// Root conversation worker, as started by the composer's conversation mode.
    pub conversation: bool,
    /// Delegated by a parent run; child runs never see `escalate`.
    pub child: bool,
    pub workspace_mode: WorkspaceMode,
    /// Lessons captured for the project, appended to the first user message.
    pub memory: Option<MemoryBoundary>,
}

impl BaseContextRequest {
    /// A root run of `role` in the shared workspace with no category or skills.
    pub fn new(role: Role) -> Self {
        Self {
            role,
            category: None,
            load_skills: Vec::new(),
            conversation: false,
            child: false,
            workspace_mode: WorkspaceMode::Shared,
            memory: None,
        }
    }
}

/// The base context a run would start with, by section, with provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct BaseContextReport {
    pub role: Role,
    pub category: Option<String>,
    /// `profile/model`, or `unresolved:<name>` when routing cannot resolve it.
    pub selected_model: String,
    pub context_window: u64,
    pub window_source: event_bus::WindowSource,
    /// Logical model and generation overrides; `None` for fixed models.
    pub binding: Option<config::types::agents::ResolvedAgentBinding>,
    pub sections: Vec<ContextSection>,
    pub tools: Vec<ContextTool>,
    pub excluded_tools: Vec<ContextExclusion>,
    pub exclusions: Vec<ContextExclusion>,
    pub available_skills: Vec<AvailableSkill>,
}

impl BaseContextReport {
    pub fn section_tokens(&self) -> u64 {
        self.sections
            .iter()
            .map(|section| section.estimated_tokens)
            .sum()
    }

    pub fn tool_tokens(&self) -> u64 {
        self.tools.iter().map(|tool| tool.estimated_tokens).sum()
    }

    /// Estimated input tokens before the task prompt and conversation.
    pub fn total_tokens(&self) -> u64 {
        self.section_tokens().saturating_add(self.tool_tokens())
    }
}

impl AgentRuntime {
    /// Compose the base context a new run of `request` would receive, without starting it.
    ///
    /// Uses the same composition as the agent loop, so the sections join into the exact
    /// initial system message. Token counts are bytes / 4 estimates.
    ///
    /// # Errors
    /// Returns [`RuntimeError::Model`] when the system prompt cannot be composed
    /// (unknown category, missing skill), mirroring the run's fail-closed start.
    pub async fn preview_base_context(
        &self,
        request: BaseContextRequest,
    ) -> Result<BaseContextReport, RuntimeError> {
        let mut shared =
            crate::runtime::loop_shared(&Arc::downgrade(&self.shared)).ok_or_else(|| {
                RuntimeError::Model {
                    reason: "runtime is shutting down".into(),
                }
            })?;
        if let Some(resolution) = self.shared.model_resolution.get() {
            resolution.apply(&mut shared.compaction).await;
        }
        let category = request.category.as_deref();
        let selected_model = shared.model.selected_model(request.role, category);
        let config = RunConfig {
            category: request.category.clone(),
            load_skills: request.load_skills.clone(),
            conversation: request.conversation,
            workspace_mode: request.workspace_mode,
            ..RunConfig::default()
        };
        let policy = self
            .execution_policy(request.role)
            .for_run_config(&config, !request.child);
        let (tools, excluded_tools) =
            self.preview_tools(&shared, &policy, &request, &selected_model);
        let sandbox_root = self
            .shared
            .sandbox_root
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let active_root = match request.workspace_mode {
            WorkspaceMode::Shared => shared_active_root(shared.rules.as_deref(), sandbox_root),
            WorkspaceMode::Isolated => None,
        };
        let mut sections = compose_system_sections(
            &shared,
            &SystemInputs {
                role: request.role,
                category,
                load_skills: &request.load_skills,
                prompt_len: 0,
                active_root: active_root.as_deref(),
            },
        )
        .map_err(|error| RuntimeError::Model {
            reason: error.to_string(),
        })?;
        resolve_preset_origins(
            &mut sections,
            self.shared
                .skill_source
                .get()
                .and_then(|source| source.user_config_dir()),
        );
        if let Some(root) = &active_root {
            sections.push(ContextSection::new(
                ContextSectionKind::Workspace,
                ContextSource::Generated,
                workspace_note(root),
            ));
        }
        if let Some(memory) = request
            .memory
            .as_ref()
            .filter(|memory| !memory.entries().is_empty())
        {
            sections.push(ContextSection::new(
                ContextSectionKind::Memory,
                ContextSource::Generated,
                memory.augment(String::new()).trim_start().to_owned(),
            ));
        }
        let (context_window, window_source) = crate::compaction::policy::resolve_window(
            &shared.compaction,
            &selected_model,
            shared.model.catalog_context_window(&selected_model),
        );
        let binding = shared.model.binding_preview(request.role, category);
        let available_skills = shared
            .skills
            .as_ref()
            .map(|registry| registry.available_skills())
            .unwrap_or_default();
        let exclusions = exclusions(&ExclusionInputs {
            shared: &shared,
            request: &request,
            active_root: active_root.as_deref(),
            binding: binding.as_ref(),
            available_skills: available_skills.len(),
        });
        Ok(BaseContextReport {
            role: request.role,
            category: request.category,
            selected_model,
            context_window,
            window_source,
            binding,
            sections,
            tools,
            excluded_tools,
            exclusions,
            available_skills,
        })
    }

    fn preview_tools(
        &self,
        shared: &LoopShared,
        policy: &ExecutionPolicy,
        request: &BaseContextRequest,
        selected_model: &str,
    ) -> (Vec<ContextTool>, Vec<ContextExclusion>) {
        let all = standard_tool_specs(&shared.executor);
        let web_tools_enabled = self.web_tools_enabled();
        let mut visible = visible_tool_specs(
            all.clone(),
            policy,
            shared.skills.is_some(),
            request.child,
            web_tools_enabled,
        );
        append_subagent_context_note(&mut visible, classify(selected_model));
        let excluded = all
            .iter()
            .filter(|spec| !visible.iter().any(|shown| shown.name == spec.name))
            .map(|spec| ContextExclusion {
                item: spec.name.clone(),
                reason: tool_exclusion_reason(
                    spec,
                    policy,
                    shared.skills.is_some(),
                    web_tools_enabled,
                )
                .into(),
            })
            .collect();
        let tools = visible
            .into_iter()
            .map(|spec| ContextTool {
                estimated_tokens: estimate_tool_tokens(std::slice::from_ref(&spec)),
                name: spec.name,
                description: spec.description,
                input_schema: spec.input_schema,
            })
            .collect();
        (tools, excluded)
    }
}

fn tool_exclusion_reason(
    spec: &ToolSpec,
    policy: &ExecutionPolicy,
    skills_configured: bool,
    web_tools_enabled: bool,
) -> &'static str {
    if policy.authorize(&spec.name).is_err() {
        "Not allowed for this role"
    } else if spec.name == "skill_load" && !skills_configured {
        "No skill registry is connected"
    } else if spec.name == "escalate" {
        "Only root runs can escalate"
    } else if !web_tools_enabled {
        "Web tools are disabled in sandbox settings"
    } else {
        "Hidden by the runtime"
    }
}

struct ExclusionInputs<'a> {
    shared: &'a LoopShared,
    request: &'a BaseContextRequest,
    active_root: Option<&'a Path>,
    binding: Option<&'a config::types::agents::ResolvedAgentBinding>,
    available_skills: usize,
}

fn exclusions(inputs: &ExclusionInputs<'_>) -> Vec<ContextExclusion> {
    let mut out = vec![ContextExclusion {
        item: "Task prompt".into(),
        reason: "The user input or delegate instruction is sent as the first user message; it differs per run.".into(),
    }];
    let mut push = |item: &str, reason: String| {
        out.push(ContextExclusion {
            item: item.into(),
            reason,
        });
    };
    if let Some(source) = inputs.shared.rules.as_deref() {
        for file in startup_rule_files(source, inputs.active_root) {
            if file.excluded == Some(StartupRuleExclusion::ProjectUntrusted) {
                push(
                    &file.path.display().to_string(),
                    "The project is not trusted, so project AGENTS.md and path-scoped rules are not injected.".into(),
                );
            }
        }
    }
    if inputs.request.workspace_mode == WorkspaceMode::Isolated {
        push(
            "Workspace note",
            "The isolated worktree path is decided when the run starts.".into(),
        );
    }
    if inputs.request.load_skills.is_empty() && inputs.available_skills > 0 {
        push(
            "Skill bodies",
            format!(
                "Bodies are injected only for delegate load_skills; otherwise the model loads them with skill_load ({} available).",
                inputs.available_skills
            ),
        );
    }
    if !inputs.shared.compaction_configured {
        push(
            "Compaction policy",
            "Compaction guidance is not configured on this runtime.".into(),
        );
    }
    if inputs
        .binding
        .is_some_and(|binding| binding.generation.top_p.is_some())
    {
        push(
            "top_p",
            "Configured, but not forwarded to provider requests.".into(),
        );
    }
    if matches!(inputs.request.role, Role::Orchestrator | Role::Worker) {
        push(
            "Team tools",
            "Team runs add task_claim, task_complete and finding_append.".into(),
        );
    }
    push(
        "Mid-run messages",
        "Path-scoped rules, agent messages, answers and runtime notices arrive later as user messages.".into(),
    );
    out
}
