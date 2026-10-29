//! The Agent features settings: on/off switches for agent tool features.
//!
//! Each row of [`AGENT_FEATURES`] describes one switch. To offer another tool
//! feature, add its field to [`config::AgentFeatures`] (and its save key) and a
//! row here; the modal renders every row.

/// One switchable agent tool feature.
pub struct AgentFeature {
    pub label: &'static str,
    pub description: &'static str,
    /// What a saved change affects, shown under the description.
    pub applies: &'static str,
    pub switch: fn(&mut config::AgentFeatures) -> &mut bool,
}

impl AgentFeature {
    pub fn default_on(&self) -> bool {
        *(self.switch)(&mut config::AgentFeatures::default())
    }
}

pub const AGENT_FEATURES: &[AgentFeature] = &[
    AgentFeature {
        label: "Web tools",
        description: "Web search and fetch. Enabled tools follow each role's tool visibility. Search runs automatically; fetch requests are reviewed using their arguments.",
        applies: "Enabling exposes the tools to runs started after saving. Disabling also blocks Web tool calls in runs already in progress.",
        switch: |features| &mut features.web_tools_enabled,
    },
    AgentFeature {
        label: "run_js (prototype)",
        description: "Lets an agent run one JavaScript program that calls several tools in a single step. An experimental prototype.",
        applies: "Enabling offers it to runs started after saving; runs already in progress keep their tool list. Disabling also refuses run_js calls in runs already in progress.",
        switch: |features| &mut features.run_js_enabled,
    },
];

/// The loaded and edited switches of the Agent features modal.
#[derive(Default)]
pub struct AgentFeaturesSettings {
    pub open: bool,
    pub saved: config::AgentFeatures,
    pub draft: config::AgentFeatures,
    pub error: Option<String>,
}

impl AgentFeaturesSettings {
    pub fn seed_from_config(config: &config::Config) -> Self {
        let features = config::AgentFeatures::from_config(config);
        Self {
            open: false,
            saved: features,
            draft: features,
            error: None,
        }
    }
}
