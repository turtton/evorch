use runtime::compose::ProfileSummary;
use workspace_ui::ModelPreference;

/// Which step of the picker modal is showing.
#[derive(Debug, Default, Clone, PartialEq)]
pub enum PickerStage {
    /// Searching the provider/model list.
    #[default]
    Models,
    /// Choosing the reasoning effort of the model just picked.
    Effort {
        model: ModelPreference,
        selected: usize,
    },
}

/// State of the model picker modal, shared by every composer.
#[derive(Debug, Default)]
pub struct ModelPickerState {
    pub open: bool,
    pub query: String,
    /// Highlighted row of the filtered model list.
    pub selected: usize,
    pub stage: PickerStage,
    /// The search box takes focus on the next frame, right after opening.
    pub focus_query: bool,
}

impl ModelPickerState {
    /// Opens the modal on the model list with the current model highlighted.
    pub fn open(&mut self, current: Option<&ModelPreference>, profiles: &[ProfileSummary]) {
        self.open = true;
        self.query.clear();
        self.stage = PickerStage::Models;
        self.focus_query = true;
        self.selected = picker_rows(profiles, "")
            .iter()
            .position(|row| match (row, current) {
                (PickerRow::Automatic, None) => true,
                (PickerRow::Model(option), Some(current)) => {
                    same_model(&option.preference, current)
                }
                _ => false,
            })
            .unwrap_or(0);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.stage = PickerStage::Models;
    }

    /// Applies a row of the model list. Returns the new selection once it is final;
    /// a model that offers reasoning effort moves to the effort step instead.
    pub fn choose_row(
        &mut self,
        row: &PickerRow,
        current: Option<&ModelPreference>,
        profiles: &[ProfileSummary],
    ) -> Option<Option<ModelPreference>> {
        match row {
            PickerRow::Automatic => {
                self.close();
                Some(None)
            }
            PickerRow::Model(option) => {
                let model = switch_model(current, option.preference.clone(), profiles);
                let efforts = effort_rows(&model, profiles);
                if efforts.len() <= 1 {
                    self.close();
                    return Some(Some(model));
                }
                let selected = efforts
                    .iter()
                    .position(|effort| *effort == model.reasoning_effort)
                    .unwrap_or(0);
                self.stage = PickerStage::Effort { model, selected };
                None
            }
        }
    }

    /// Applies the highlighted effort of the effort step.
    pub fn choose_effort(
        &mut self,
        profiles: &[ProfileSummary],
        index: usize,
    ) -> Option<Option<ModelPreference>> {
        let PickerStage::Effort { model, .. } = &self.stage else {
            return None;
        };
        let reasoning_effort = effort_rows(model, profiles).get(index)?.clone();
        let selected = ModelPreference {
            reasoning_effort,
            ..model.clone()
        };
        self.close();
        Some(Some(selected))
    }
}

/// A selectable model with the provider it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelOption {
    pub preference: ModelPreference,
    pub provider: &'static str,
}

/// One row of the model list.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRow {
    Automatic,
    Model(ModelOption),
}

/// Provider type as written in the configuration, for display and search.
pub fn provider_label(provider_type: model::ProviderType) -> &'static str {
    match provider_type {
        model::ProviderType::Anthropic => "anthropic",
        model::ProviderType::AnthropicSubscription => "anthropic-subscription",
        model::ProviderType::OpenAi => "openai",
        model::ProviderType::OpenAiCodex => "openai-codex",
        model::ProviderType::GithubCopilot => "github-copilot",
        model::ProviderType::Openrouter => "openrouter",
        model::ProviderType::OpenAiCompatible => "openai-compatible",
        model::ProviderType::KimiSubscription => "kimi-subscription",
        model::ProviderType::Cursor => "cursor",
    }
}

/// Rows matching `query`, best match first. Every word of the query must match the
/// profile name, the provider type or the model name; an empty query lists
/// everything with automatic routing first.
pub fn picker_rows(profiles: &[ProfileSummary], query: &str) -> Vec<PickerRow> {
    let mut rows: Vec<(u32, PickerRow)> = Vec::new();
    if let Some(score) = super::fuzzy::score(["automatic routing", "auto"], query) {
        rows.push((score, PickerRow::Automatic));
    }
    for preference in profile_options(profiles) {
        let provider = profiles
            .iter()
            .find(|profile| profile.name == preference.profile)
            .map_or("", |profile| provider_label(profile.provider_type));
        let model = preference.model.as_deref().unwrap_or("default");
        let joined = format!("{}/{model}", preference.profile);
        let fields = [
            preference.profile.as_str(),
            provider,
            model,
            joined.as_str(),
        ];
        if let Some(score) = super::fuzzy::score(fields, query) {
            rows.push((
                score,
                PickerRow::Model(ModelOption {
                    preference,
                    provider,
                }),
            ));
        }
    }
    rows.sort_by_key(|(score, _)| *score);
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Rows of the effort step: the default effort, then each level the model offers
/// (and the current custom effort, so it stays visible).
pub fn effort_rows(
    preference: &ModelPreference,
    profiles: &[ProfileSummary],
) -> Vec<Option<String>> {
    let choices = effort_choices(preference, profiles);
    if choices.is_empty() {
        return vec![None];
    }
    let mut rows = vec![None];
    rows.extend(choices.into_iter().map(Some));
    if let Some(custom) = preference
        .reasoning_effort
        .as_ref()
        .filter(|effort| !rows.contains(&Some((*effort).clone())))
    {
        rows.push(Some(custom.clone()));
    }
    rows
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
    // Cursor encodes effort in sibling model IDs; the picker does not resolve those.
    if profile.is_some_and(|profile| profile.provider_type == model::ProviderType::Cursor) {
        return Vec::new();
    }
    let selected_model = preference
        .model
        .as_ref()
        .or_else(|| profile.and_then(|profile| profile.default_model.as_ref()));
    let levels = profile.and_then(|profile| profile.effort_levels.get(selected_model?));
    // The Anthropic API client does not forward effort yet, so only the levels a model
    // lists explicitly are offered, never the common defaults.
    if profile.is_some_and(|profile| profile.provider_type == model::ProviderType::Anthropic) {
        return levels.cloned().unwrap_or_default();
    }
    if profile
        .is_some_and(|profile| profile.provider_type == model::ProviderType::AnthropicSubscription)
    {
        return super::effort::claude_effort_choices(
            selected_model.map_or("", String::as_str),
            levels.map(Vec::as_slice),
        );
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        name: &str,
        provider_type: model::ProviderType,
        models: &[&str],
        effort_levels: &[(&str, &[&str])],
    ) -> ProfileSummary {
        ProfileSummary {
            name: name.into(),
            provider_type,
            models: models.iter().map(|model| (*model).to_owned()).collect(),
            default_model: models.first().map(|model| (*model).to_owned()),
            effort_levels: effort_levels
                .iter()
                .map(|(model, levels)| {
                    (
                        (*model).to_owned(),
                        levels.iter().map(|level| (*level).to_owned()).collect(),
                    )
                })
                .collect(),
        }
    }

    fn preference(profile: &str, model: &str) -> ModelPreference {
        ModelPreference {
            profile: profile.into(),
            model: Some(model.into()),
            reasoning_effort: None,
        }
    }

    fn labels(rows: &[PickerRow]) -> Vec<String> {
        rows.iter()
            .map(|row| match row {
                PickerRow::Automatic => "auto".into(),
                PickerRow::Model(option) => preference_label(&option.preference),
            })
            .collect()
    }

    #[test]
    fn rows_rank_prefix_before_contains_and_search_provider_fields() {
        let profiles = [
            profile(
                "work",
                model::ProviderType::Anthropic,
                &["claude-opus"],
                &[],
            ),
            profile(
                "claude-max",
                model::ProviderType::AnthropicSubscription,
                &["haiku"],
                &[],
            ),
            profile(
                "gw",
                model::ProviderType::OpenAiCompatible,
                &["opus-like"],
                &[],
            ),
        ];
        // (query, expected rows in order)
        for (query, expected) in [
            (
                "",
                vec![
                    "auto",
                    "work / claude-opus",
                    "claude-max / haiku",
                    "gw / opus-like",
                ],
            ),
            (
                "opus",
                vec!["gw / opus-like", "work / claude-opus", "claude-max / haiku"],
            ),
            ("claude", vec!["work / claude-opus", "claude-max / haiku"]),
            ("subscription", vec!["claude-max / haiku"]),
            ("openai-compatible opus", vec!["gw / opus-like"]),
            ("work/claude", vec!["work / claude-opus"]),
            ("xyz", vec![]),
        ] {
            assert_eq!(
                labels(&picker_rows(&profiles, query)),
                expected,
                "{query:?}"
            );
        }
    }

    #[test]
    fn effort_choices_by_provider_type() {
        let levels: &[(&str, &[&str])] = &[("m", &["low", "high"])];
        let cases = [
            // Anthropic API: explicit levels only, never the common defaults.
            (model::ProviderType::Anthropic, "m", vec!["low", "high"]),
            (model::ProviderType::Anthropic, "unlisted", vec![]),
            // Cursor never offers effort: it would need sibling-ID resolution.
            (model::ProviderType::Cursor, "m", vec![]),
            (
                model::ProviderType::OpenAiCompatible,
                "m",
                vec!["low", "high"],
            ),
        ];
        for (provider_type, model_id, expected) in cases {
            let profiles = [profile("p", provider_type, &["m", "unlisted"], levels)];
            assert_eq!(
                effort_choices(&preference("p", model_id), &profiles),
                expected,
                "{provider_type:?} {model_id}"
            );
        }
    }

    #[test]
    fn choosing_a_row_applies_directly_or_moves_to_the_effort_step() {
        let profiles = [
            profile(
                "p",
                model::ProviderType::OpenAiCompatible,
                &["plain", "deep"],
                &[("deep", &["low", "high"])],
            ),
            profile("c", model::ProviderType::Cursor, &["composer-2.5"], &[]),
        ];
        let row = |profile: &str, model: &str| {
            PickerRow::Model(ModelOption {
                preference: preference(profile, model),
                provider: "",
            })
        };
        let mut state = ModelPickerState::default();
        state.open(None, &profiles);
        // A model without effort levels is final at once.
        assert_eq!(
            state.choose_row(&row("c", "composer-2.5"), None, &profiles),
            Some(Some(preference("c", "composer-2.5")))
        );
        assert!(!state.open);
        // A model with levels waits for the effort step, which starts on the kept effort.
        state.open(None, &profiles);
        let current = ModelPreference {
            reasoning_effort: Some("high".into()),
            ..preference("p", "plain")
        };
        assert_eq!(
            state.choose_row(&row("p", "deep"), Some(&current), &profiles),
            None
        );
        assert_eq!(
            state.stage,
            PickerStage::Effort {
                model: ModelPreference {
                    reasoning_effort: Some("high".into()),
                    ..preference("p", "deep")
                },
                selected: 2,
            }
        );
        // Index 0 is Default effort, which clears it.
        assert_eq!(
            state.choose_effort(&profiles, 0),
            Some(Some(preference("p", "deep")))
        );
        assert!(!state.open);
        // Automatic routing is final at once.
        state.open(Some(&current), &profiles);
        assert_eq!(
            state.choose_row(&PickerRow::Automatic, None, &profiles),
            Some(None)
        );
    }

    #[test]
    fn opening_highlights_the_current_model() {
        let profiles = [profile(
            "p",
            model::ProviderType::OpenAiCompatible,
            &["a", "b"],
            &[],
        )];
        let mut state = ModelPickerState::default();
        state.open(Some(&preference("p", "b")), &profiles);
        assert_eq!(state.selected, 2);
        state.open(None, &profiles);
        assert_eq!(state.selected, 0);
    }

    #[test]
    fn claude_effort_choices_resolve_default_and_restrict_model_overrides() {
        let profiles = [ProfileSummary {
            name: "claude".into(),
            provider_type: model::ProviderType::AnthropicSubscription,
            models: vec![],
            default_model: Some("claude-sonnet-5-5".into()),
            effort_levels: [
                (
                    "claude-sonnet-4-5".into(),
                    vec!["high".into(), "minimal".into(), "low".into(), "max".into()],
                ),
                ("claude-opus-4-7".into(), vec![]),
                ("unknown".into(), vec!["high".into()]),
            ]
            .into(),
        }];
        for (model, expected) in [
            (None, vec!["low", "medium", "high", "xhigh", "max"]),
            (Some("claude-sonnet-4-5"), vec!["high", "low"]),
            (Some("claude-opus-4-7"), vec![]),
            (Some("unknown"), vec![]),
        ] {
            let preference = ModelPreference {
                profile: "claude".into(),
                model: model.map(str::to_owned),
                reasoning_effort: None,
            };
            assert_eq!(
                effort_choices(&preference, &profiles),
                expected,
                "{model:?}"
            );
        }
    }
}
