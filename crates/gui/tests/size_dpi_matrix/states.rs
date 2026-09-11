use gui::app::WorkbenchState;
use gui::fixture::{
    DemoSource, demo_error_events, demo_provider_config, demo_runs, demo_sidebar, populate,
};
use gui::model::provider_settings::ProviderSettingsModel;
use workspace_ui::UiSettings;

#[derive(Clone, Copy, Debug)]
pub enum State {
    Empty,
    Demo,
    ErrorThread,
    EditProfile,
}

impl State {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::Demo => "demo",
            Self::ErrorThread => "error-thread",
            Self::EditProfile => "edit-profile",
        }
    }

    pub fn build(self, root: &std::path::Path) -> WorkbenchState<DemoSource> {
        let mut state = match self {
            Self::Empty => WorkbenchState::new(DemoSource(Vec::new()), &UiSettings::default())
                .expect("empty state builds"),
            Self::Demo | Self::ErrorThread | Self::EditProfile => populate(
                WorkbenchState::new(DemoSource(demo_runs()), &UiSettings::default())
                    .expect("demo state builds"),
                demo_sidebar(root).expect("demo sidebar builds"),
            )
            .with_provider_settings_path(root.join("evorch.toml")),
        };
        match self {
            Self::Empty | Self::Demo => {}
            Self::ErrorThread => state.apply_events(demo_error_events()),
            Self::EditProfile => {
                *state.provider_settings_mut() =
                    ProviderSettingsModel::seed_from_config(&demo_provider_config());
                state.provider_settings_mut().open = true;
                state.provider_settings_mut().edit("local");
            }
        }
        state
    }
}
