//! The Agent features settings: on/off switches for agent tool features.
//!
//! Each row of [`AGENT_FEATURES`] describes one switch. To offer another tool
//! feature, add its field to [`config::AgentFeatures`] (and its save key) and a
//! row here; the modal renders every row.

/// What a feature edits besides its switch.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum FeatureDetail {
    /// `tools.run_js.scopes`: the roles and categories the tool is offered to.
    RunJsScopes,
}

/// One switchable agent tool feature.
pub struct AgentFeature {
    pub label: &'static str,
    pub description: &'static str,
    /// What a saved change affects, shown under the description.
    pub applies: &'static str,
    pub switch: fn(&mut config::AgentFeatures) -> &mut bool,
    pub detail: Option<FeatureDetail>,
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
        detail: None,
    },
    AgentFeature {
        label: "run_js (prototype)",
        description: "Lets an agent run one JavaScript program that calls several tools in a single step. An experimental prototype, offered only to the roles and categories chosen below.",
        applies: "Enabling offers it to runs started after saving; runs already in progress keep their tool list. Disabling, or removing a run's role or category below, also refuses run_js calls in runs already in progress.",
        switch: |features| &mut features.run_js_enabled,
        detail: Some(FeatureDetail::RunJsScopes),
    },
];

/// A role that can be given `run_js`, with the categories a scope may name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeRole {
    /// The `agents.<role>` key, which is also the role scope.
    pub key: &'static str,
    /// Every category registered for the role, as `<role>:<category>` scopes.
    pub categories: Vec<&'static str>,
}

impl ScopeRole {
    pub fn category_scope(&self, category: &str) -> String {
        format!("{}:{category}", self.key)
    }
}

/// The roles whose capability includes `run_js`: only those can be offered it,
/// whatever the scopes say.
pub fn run_js_scope_roles() -> Vec<ScopeRole> {
    use agents::Role;
    [
        Role::Worker,
        Role::Reviewer,
        Role::Explorer,
        Role::WebResearcher,
        Role::Planner,
        Role::Advisor,
    ]
    .into_iter()
    .filter(|role| role.capabilities().allowed_tools.contains("run_js"))
    .map(|role| ScopeRole {
        key: role.config_key(),
        categories: config::agent_categories::CategoryId::ALL
            .iter()
            .filter(|category| category.role() == role.config_key())
            .map(|category| category.as_str())
            .collect(),
    })
    .collect()
}

/// Whether `scope` is listed.
pub fn scope_listed(scopes: &[String], scope: &str) -> bool {
    scopes.iter().any(|listed| listed == scope)
}

/// Lists or unlists `scope`. Listing a role drops its `role:category` entries,
/// which it already covers; unlisting leaves the others as they are.
pub fn set_scope(scopes: &mut Vec<String>, scope: &str, listed: bool) {
    if listed {
        if scope_listed(scopes, scope) {
            return;
        }
        if !scope.contains(':') {
            let prefix = format!("{scope}:");
            scopes.retain(|existing| !existing.starts_with(&prefix));
        }
        scopes.push(scope.to_owned());
    } else {
        scopes.retain(|existing| existing != scope);
    }
}

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
            saved: features.clone(),
            draft: features,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_roles_with_the_run_js_capability_are_listed_with_their_categories() {
        let roles = run_js_scope_roles();
        let keys: Vec<_> = roles.iter().map(|role| role.key).collect();
        assert_eq!(
            keys,
            [
                "worker",
                "reviewer",
                "explorer",
                "web_researcher",
                "planner",
                "advisor"
            ]
        );
        let worker = &roles[0];
        assert!(worker.categories.contains(&"deep"));
        assert!(worker.categories.contains(&"high-reasoning"));
        assert!(roles[2].categories.is_empty());
        for role in &roles {
            for category in &role.categories {
                config::validate_run_js_scope(&role.category_scope(category)).unwrap();
            }
        }
    }

    #[test]
    fn listing_a_role_replaces_its_category_entries_and_unlisting_keeps_the_rest() {
        let mut scopes = vec!["worker:deep".to_owned(), "reviewer:plan-review".to_owned()];

        set_scope(&mut scopes, "worker", true);
        assert_eq!(scopes, ["reviewer:plan-review", "worker"]);
        set_scope(&mut scopes, "worker", true);
        assert_eq!(scopes, ["reviewer:plan-review", "worker"]);

        set_scope(&mut scopes, "worker", false);
        assert_eq!(scopes, ["reviewer:plan-review"]);
        set_scope(&mut scopes, "worker:quick", true);
        assert_eq!(scopes, ["reviewer:plan-review", "worker:quick"]);
    }
}
