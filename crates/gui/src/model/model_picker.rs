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
                }]
            } else {
                models
                    .into_iter()
                    .map(|model| ModelPreference {
                        profile: profile.name.clone(),
                        model: Some(model),
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
