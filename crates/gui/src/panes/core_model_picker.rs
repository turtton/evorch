//! Protocol-only surface around the unchanged embedded model picker.
use crate::model::model_picker::ModelPickerState;
use runtime::compose::ProfileSummary;
use workbench_protocol::commands::{ModelChoice, ModelPreference};
use workbench_protocol::namespaces::settings::{ProjectModelChoices, ProviderKind};

/// Widest the model button grows in the composer; a longer model name is elided.
pub const MAX_BUTTON_WIDTH: f32 = 260.0;

#[derive(Default)]
pub struct CoreModelPicker {
    state: ModelPickerState,
}

pub fn profiles(catalog: &ProjectModelChoices) -> Vec<ProfileSummary> {
    catalog
        .profiles
        .iter()
        .map(|p| ProfileSummary {
            name: p.name.clone(),
            provider_type: match p.provider_type {
                ProviderKind::Anthropic => model::ProviderType::Anthropic,
                ProviderKind::AnthropicSubscription => model::ProviderType::AnthropicSubscription,
                ProviderKind::Openai => model::ProviderType::OpenAi,
                ProviderKind::OpenaiCodex => model::ProviderType::OpenAiCodex,
                ProviderKind::GithubCopilot => model::ProviderType::GithubCopilot,
                ProviderKind::Openrouter => model::ProviderType::Openrouter,
                ProviderKind::OpenaiCompatible => model::ProviderType::OpenAiCompatible,
                ProviderKind::KimiSubscription => model::ProviderType::KimiSubscription,
                ProviderKind::Cursor => model::ProviderType::Cursor,
            },
            models: p.models.clone(),
            default_model: p.default_model.clone(),
            effort_levels: p.effort_levels.clone(),
        })
        .collect()
}

fn preference(choice: &ModelChoice) -> Option<workspace_ui::ModelPreference> {
    match choice {
        ModelChoice::RoleDefault => None,
        ModelChoice::Explicit(p) => Some(workspace_ui::ModelPreference {
            profile: p.profile.clone(),
            model: p.model.clone(),
            reasoning_effort: p.reasoning_effort.clone(),
        }),
    }
}

fn choice(preference: Option<workspace_ui::ModelPreference>) -> ModelChoice {
    preference.map_or(ModelChoice::RoleDefault, |p| {
        ModelChoice::Explicit(ModelPreference {
            profile: p.profile,
            model: p.model,
            reasoning_effort: p.reasoning_effort,
        })
    })
}

impl CoreModelPicker {
    pub fn close(&mut self) {
        self.state.close();
    }

    pub fn show(
        &mut self,
        ui: &mut egui::Ui,
        catalog: Option<&ProjectModelChoices>,
        selected: &ModelChoice,
        orchestrator: bool,
        enabled: bool,
    ) -> Option<ModelChoice> {
        let profiles = catalog.map(profiles).unwrap_or_default();
        let preference = preference(selected);
        let default = catalog.map(|c| {
            if orchestrator {
                &c.orchestrator
            } else {
                &c.worker
            }
        });
        let default_label = default.map(|d| match &d.reasoning_effort {
            Some(effort) => format!("{} · {effort}", d.model),
            None => d.model.clone(),
        });
        // The note that used to be a line under the button is its hover text, so the
        // picker takes no row of its own beside the role and team buttons.
        let hover_note = if matches!(selected, ModelChoice::RoleDefault) {
            "Role default · Applies to next message"
        } else {
            "Applies to next message"
        };
        super::model_picker::model_picker_with(
            ui,
            super::model_picker::ModelPickerContext {
                profiles: &profiles,
                preference: preference.as_ref(),
                default_model: default_label.as_deref(),
                enabled,
            },
            &mut self.state,
            super::model_picker::ButtonStyle {
                max_width: Some(MAX_BUTTON_WIDTH),
                hover_note: Some(hover_note),
            },
        )
        .map(choice)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn protocol_choices_preserve_profile_default_and_custom_effort() {
        for value in [
            ModelChoice::RoleDefault,
            ModelChoice::Explicit(ModelPreference {
                profile: "p".into(),
                model: None,
                reasoning_effort: Some("custom".into()),
            }),
        ] {
            assert_eq!(choice(preference(&value)), value);
        }
    }
}
