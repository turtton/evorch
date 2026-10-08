use runtime::compose::ProfileSummary;
use workspace_ui::ModelPreference;

#[derive(Debug, Default)]
pub struct ModelPickerState {
    pub open: bool,
    pub hovered: bool,
}

pub fn preference_label(preference: &ModelPreference) -> String {
    format!(
        "{} / {}",
        preference.profile,
        preference.model.as_deref().unwrap_or("default")
    )
}

pub fn profile_options(profiles: &[ProfileSummary]) -> Vec<ModelPreference> {
    profiles
        .iter()
        .flat_map(|profile| {
            let mut models = profile.models.clone();
            if let Some(default) = &profile.default_model
                && !models.contains(default)
            {
                models.push(default.clone());
            }
            if models.is_empty() {
                vec![ModelPreference {
                    profile: profile.name.clone(),
                    model: None,
                    reasoning_effort: None,
                }]
            } else {
                models
                    .into_iter()
                    .map(|model| ModelPreference {
                        profile: profile.name.clone(),
                        model: Some(model),
                        reasoning_effort: None,
                    })
                    .collect()
            }
        })
        .collect()
}

pub fn parse_preference(label: &str, profiles: &[ProfileSummary]) -> Option<ModelPreference> {
    profile_options(profiles)
        .into_iter()
        .find(|preference| preference_label(preference) == label)
}

/// Whether two preferences select the same profile and model, ignoring effort.
pub fn same_model(a: &ModelPreference, b: &ModelPreference) -> bool {
    a.profile == b.profile && a.model == b.model
}

/// Effort levels offered for the preference's model (explicit or profile default).
pub fn effort_choices(preference: &ModelPreference, profiles: &[ProfileSummary]) -> Vec<String> {
    let profile = profiles
        .iter()
        .find(|profile| profile.name == preference.profile);
    if profile.is_some_and(|profile| {
        matches!(
            profile.provider_type,
            model::ProviderType::Anthropic
                | model::ProviderType::AnthropicSubscription
                | model::ProviderType::Cursor
        )
    }) {
        return Vec::new();
    }
    let levels = profile.and_then(|profile| {
        let model = preference
            .model
            .as_ref()
            .or(profile.default_model.as_ref())?;
        profile.effort_levels.get(model)
    });
    super::effort::effort_choices(levels.map(Vec::as_slice))
}

/// Selects `next`, keeping the current effort only when the new model offers it.
pub fn switch_model(
    current: Option<&ModelPreference>,
    mut next: ModelPreference,
    profiles: &[ProfileSummary],
) -> ModelPreference {
    next.reasoning_effort = current
        .and_then(|current| current.reasoning_effort.clone())
        .filter(|effort| effort_choices(&next, profiles).contains(effort));
    next
}
