//! Settings namespace adapter: translates the protocol's operations into the
//! `settings` area service (`self.areas.settings`). Every command is new work
//! for a planned restart: it writes the configuration file or recomposes the
//! models, so a preparing host refuses it, and one that was accepted finishes
//! before the host seals (bounded I/O, nothing waits for a person).
use workbench_protocol::commands::{CommandDisposition, CommandReply, CommandResult};
use workbench_protocol::envelope::Provenance;
use workbench_protocol::errors::WorkbenchError;
use workbench_protocol::namespaces::settings::{
    SettingsCommand, SettingsQuery, SettingsReply, SettingsResult,
};
use workbench_protocol::queries::{Query, QueryReply};

use crate::ingress::CommandHost;

impl CommandHost {
    pub(in crate::ingress) fn settings_command(
        &mut self,
        command: SettingsCommand,
        _provenance: &Provenance,
        restart: crate::bootstrap::restart::HostCommandPermit,
    ) -> Result<CommandReply, WorkbenchError> {
        let submitted = self.areas.settings.submit(command, Some(restart))?;
        let mut reply = CommandReply::with_result(CommandResult::Settings(
            SettingsResult::Operation(submitted.view),
        ));
        if submitted.already_applied {
            reply.disposition = CommandDisposition::AlreadyApplied;
        }
        Ok(reply)
    }

    pub(in crate::ingress) fn settings_query(
        &self,
        _query: &Query,
        query: &SettingsQuery,
    ) -> Result<QueryReply, WorkbenchError> {
        let settings = &self.areas.settings;
        Ok(QueryReply::Settings(match query {
            SettingsQuery::ModelChoices { project } => {
                use workbench_protocol::namespaces::settings::{
                    ModelProfileChoice, ProjectModelChoices, ProviderKind, RoleModelChoice,
                };
                let root = self.project_root(project)?;
                let model = self.service.runtime().project_model_snapshot(&root);
                let role = |role| RoleModelChoice {
                    model: model.selected_model(role, Some("conversation")),
                    reasoning_effort: model.selected_reasoning_effort(role, Some("conversation")),
                };
                let worker = role(runtime::Role::Worker);
                let orchestrator = role(runtime::Role::Orchestrator);
                let profiles = model
                    .available_profiles()
                    .into_iter()
                    .map(|p| ModelProfileChoice {
                        name: p.name,
                        provider_type: match p.provider_type {
                            model::ProviderType::Anthropic => ProviderKind::Anthropic,
                            model::ProviderType::AnthropicSubscription => {
                                ProviderKind::AnthropicSubscription
                            }
                            model::ProviderType::OpenAi => ProviderKind::Openai,
                            model::ProviderType::OpenAiCodex => ProviderKind::OpenaiCodex,
                            model::ProviderType::GithubCopilot => ProviderKind::GithubCopilot,
                            model::ProviderType::Openrouter => ProviderKind::Openrouter,
                            model::ProviderType::OpenAiCompatible => ProviderKind::OpenaiCompatible,
                            model::ProviderType::KimiSubscription => ProviderKind::KimiSubscription,
                            model::ProviderType::Cursor => ProviderKind::Cursor,
                        },
                        models: p.models,
                        default_model: p.default_model,
                        effort_levels: p.effort_levels,
                    })
                    .collect();
                SettingsReply::ModelChoices(ProjectModelChoices {
                    project: project.clone(),
                    profiles,
                    worker,
                    orchestrator,
                })
            }
            SettingsQuery::GetSettingsSnapshot {} => {
                SettingsReply::Settings(Box::new(settings.snapshot()?))
            }
            SettingsQuery::GetSettingsOperation { operation_id } => {
                SettingsReply::Operation(settings.operation(operation_id)?)
            }
            SettingsQuery::ListSettingsOperations {} => SettingsReply::Operations {
                operations: settings.operations()?,
            },
        }))
    }
}

impl CommandHost {
    /// Every settings command writes or recomposes: new work.
    pub(in crate::ingress) fn settings_command_access(
        _command: &SettingsCommand,
    ) -> crate::bootstrap::restart::CommandAccess {
        crate::bootstrap::restart::CommandAccess::NewWork
    }
}
