//! Run-entry selection and immutable persistence; never reselected at send time.
use crate::{AgentInvocationContext, AgentModel, ModelFamily, Role, RuntimeError};
use config::model_tool_descriptions::{ModelToolDescriptions, ToolDescriptions, validate_map};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const MAX_SNAPSHOT_BYTES: usize = 128 * 1024;
const VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FrozenToolDescriptions {
    pub version: u32,
    pub profile: Option<String>,
    pub route_model_id: Option<String>,
    pub service_tier: Option<String>,
    pub descriptions: ToolDescriptions,
    pub hash: String,
    /// Intentionally frozen even when bindings change before restore.
    pub family: ModelFamily,
}
pub(crate) fn deserialize_present<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<FrozenToolDescriptions>, D::Error> {
    let snapshot = FrozenToolDescriptions::deserialize(deserializer)?;
    snapshot
        .validate()
        .map_err(|_| serde::de::Error::custom("invalid tool description snapshot"))?;
    Ok(Some(snapshot))
}
fn error(reason: &str) -> RuntimeError {
    RuntimeError::Model {
        reason: format!("model tool descriptions: {reason}"),
    }
}
impl FrozenToolDescriptions {
    pub(crate) fn select(
        settings: &ModelToolDescriptions,
        model: &dyn AgentModel,
        invocation: &AgentInvocationContext,
        role: Role,
        family: ModelFamily,
        registry: &BTreeSet<String>,
    ) -> Result<Self, RuntimeError> {
        let identity = model.tool_description_identity(invocation, role)?;
        let descriptions = identity
            .as_ref()
            .and_then(|id| settings.get(&id.profile)?.get(&id.model_id))
            .cloned()
            .unwrap_or_default();
        let (profile, route_model_id, service_tier) = identity
            .map(|id| {
                let (_, speed) = config::types::provider::parse_model_speed(&id.model_id);
                (
                    Some(id.profile),
                    Some(id.model_id),
                    (speed == config::types::provider::ModelSpeed::Fast).then(|| "priority".into()),
                )
            })
            .unwrap_or_default();
        let mut snapshot = Self {
            version: VERSION,
            profile,
            route_model_id,
            service_tier,
            descriptions,
            hash: String::new(),
            family,
        };
        snapshot.hash = snapshot.canonical_hash()?;
        snapshot.validate()?;
        if snapshot
            .descriptions
            .keys()
            .any(|key| !registry.contains(key))
        {
            return Err(error("unknown or non-fixed builtin tool"));
        }
        Ok(snapshot)
    }
    fn canonical_hash(&self) -> Result<String, RuntimeError> {
        let bytes = serde_json::to_vec(&(
            self.version,
            &self.profile,
            &self.route_model_id,
            &self.service_tier,
            &self.descriptions,
            self.family,
        ))
        .map_err(|_| error("cannot serialize snapshot"))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
    pub(crate) fn validate(&self) -> Result<(), RuntimeError> {
        validate_map(&self.descriptions).map_err(|_| error("invalid description map"))?;
        if self.version != VERSION
            || self.hash != self.canonical_hash()?
            || self.profile.is_some() != self.route_model_id.is_some()
            || [&self.profile, &self.route_model_id].iter().any(|v| {
                v.as_ref()
                    .is_some_and(|s| s.trim().is_empty() || s.chars().any(char::is_control))
            })
        {
            return Err(error("corrupt snapshot"));
        }
        let redactor = secret_guard::SecretRedactor::from_env();
        if self
            .descriptions
            .iter()
            .any(|(key, value)| redactor.detect(key).is_some() || redactor.detect(value).is_some())
        {
            return Err(error("snapshot contains secret material"));
        }
        let bytes = serde_json::to_vec(self).map_err(|_| error("cannot serialize snapshot"))?;
        if bytes.len() > MAX_SNAPSHOT_BYTES {
            return Err(error("snapshot exceeds 131072 bytes"));
        }
        let text = String::from_utf8(bytes).map_err(|_| error("snapshot is not UTF-8"))?;
        if redactor.detect(&text).is_some() {
            return Err(error("snapshot contains secret material"));
        }
        Ok(())
    }
    /// Projection only: never mutate the saved snapshot. A disappeared builtin
    /// cannot be transferred to an external tool or another public name.
    pub(crate) fn project(&self, registry: &BTreeSet<String>) -> (ToolDescriptions, Vec<String>) {
        let mut missing = Vec::new();
        let descriptions = self
            .descriptions
            .iter()
            .filter_map(|(name, description)| {
                if registry.contains(name) {
                    Some((name.clone(), description.clone()))
                } else {
                    missing.push(name.clone());
                    None
                }
            })
            .collect();
        (descriptions, missing)
    }
}

/// The fixed builtin tools a replay's executor offers, for checks that must
/// run before any provider request: which names a description may change, what
/// the build says today, and which roles can see each tool.
pub struct BuiltinCatalog {
    specs: Vec<providers::ToolSpec>,
    fixed: BTreeSet<String>,
}

impl BuiltinCatalog {
    /// The standard and web tools every evaluation replay runs with, plus the
    /// runtime's meta operations. Browser tools are off in replays.
    ///
    /// # Errors
    /// The web tools cannot be built.
    pub fn for_evaluation() -> Result<Self, RuntimeError> {
        let executor = tools::ToolExecutor::with_standard_tools(
            std::sync::Arc::new(event_bus::EventBus::new(4)),
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        )
        .with_web_tools()
        .map_err(|error_| error(&error_.to_string()))?;
        Ok(Self {
            fixed: crate::agent_loop::tool_catalog::description_registry(&executor),
            specs: crate::agent_loop::tool_catalog::raw_specs(&executor),
        })
    }

    /// The build's own description of every overridable tool, by name.
    pub fn descriptions(&self) -> std::collections::BTreeMap<String, String> {
        self.specs
            .iter()
            .filter(|spec| self.fixed.contains(&spec.name))
            .map(|spec| (spec.name.clone(), spec.description.clone()))
            .collect()
    }

    /// Whether `role` can be offered the fixed tool `name`, reading the per-run
    /// filters as permissively as they can be.
    pub fn visible_to(&self, role: Role, name: &str) -> bool {
        self.fixed.contains(name)
            && crate::agent_loop::tool_catalog::permissive_visible_specs(
                self.specs.clone(),
                &crate::ExecutionPolicy::for_role(role),
            )
            .iter()
            .any(|spec| spec.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use providers::{Message, ToolSpec};
    struct Routed {
        profile: &'static str,
        model: &'static str,
    }
    #[async_trait::async_trait]
    impl AgentModel for Routed {
        fn selected_model(&self, _: Role, _: Option<&str>) -> String {
            "legacy-family".into()
        }
        fn tool_description_identity(
            &self,
            invocation: &AgentInvocationContext,
            _: Role,
        ) -> Result<Option<routing::ResolvedRoute>, RuntimeError> {
            let preference = invocation.model_preference.as_ref();
            Ok(Some(routing::ResolvedRoute {
                profile: preference
                    .map_or(self.profile, |p| p.profile.as_str())
                    .into(),
                model_id: preference
                    .and_then(|p| p.model.as_deref())
                    .unwrap_or(self.model)
                    .into(),
            }))
        }
        async fn complete(
            &self,
            _: &AgentInvocationContext,
            _: Role,
            _: &[Message],
            _: &[ToolSpec],
        ) -> Result<providers::ChatResponse, RuntimeError> {
            panic!("selection must not call provider")
        }
    }
    fn registry() -> BTreeSet<String> {
        ["read".into(), "delegate".into()].into()
    }
    fn snapshot(map: ToolDescriptions) -> Result<FrozenToolDescriptions, RuntimeError> {
        FrozenToolDescriptions::select(
            &ModelToolDescriptions::from([("p".into(), [("m".into(), map)].into())]),
            &Routed {
                profile: "p",
                model: "m",
            },
            &AgentInvocationContext::default(),
            Role::Worker,
            ModelFamily::Gpt5,
            &registry(),
        )
    }
    #[test]
    fn profile_and_exact_model_selection_never_falls_back_from_fast() {
        let map =
            ToolDescriptions::from([("read".into(), "Read from the selected variant".into())]);
        let settings =
            ModelToolDescriptions::from([("p".into(), [("m".into(), map.clone())].into())]);
        for (profile, model, expected) in [
            ("p", "m", map.clone()),
            ("p", "m+fast", ToolDescriptions::new()),
            ("other", "m", ToolDescriptions::new()),
        ] {
            let selected = FrozenToolDescriptions::select(
                &settings,
                &Routed { profile, model },
                &AgentInvocationContext::default(),
                Role::Worker,
                ModelFamily::Gpt5,
                &registry(),
            )
            .unwrap();
            assert_eq!(selected.descriptions, expected);
            assert_eq!(selected.route_model_id.as_deref(), Some(model));
        }
    }
    #[test]
    fn snapshot_freezes_empty_map_and_family_even_after_binding_edits() {
        let empty = snapshot(ToolDescriptions::new()).unwrap();
        let encoded = serde_json::to_string(&empty).unwrap();
        let decoded: FrozenToolDescriptions = serde_json::from_str(&encoded).unwrap();
        decoded.validate().unwrap();
        assert_eq!(decoded, empty);
        assert_eq!(decoded.family, ModelFamily::Gpt5);
        assert!(decoded.descriptions.is_empty());
    }
    #[test]
    fn corruption_never_falls_back_to_current_config() {
        let mut saved = snapshot([("read".into(), "Saved description".into())].into()).unwrap();
        saved.descriptions.insert("read".into(), "tampered".into());
        assert!(saved.validate().is_err());
        saved.hash = saved.canonical_hash().unwrap();
        saved.version += 1;
        assert!(saved.validate().is_err());
    }
    #[test]
    fn removed_builtin_projection_does_not_rewrite_snapshot_or_expose_hidden_tools() {
        let saved = snapshot(
            [
                ("read".into(), "Read override".into()),
                ("delegate".into(), "Delegate override".into()),
            ]
            .into(),
        )
        .unwrap();
        let original = serde_json::to_vec(&saved).unwrap();
        let (projected, removed) = saved.project(&["delegate".into()].into());
        assert_eq!(removed, ["read"]);
        assert_eq!(projected.keys().cloned().collect::<Vec<_>>(), ["delegate"]);
        assert_eq!(serde_json::to_vec(&saved).unwrap(), original);
        assert!(snapshot([("unknown".into(), "Cannot apply".into())].into()).is_err());
        for name in ["run_js", "mcp_read", "task_claim", "external"] {
            assert!(snapshot([(name.into(), "Cannot apply".into())].into()).is_err());
        }
    }
    #[test]
    fn snapshot_limit_includes_json_escaping_and_metadata() {
        let mut saved = snapshot(ToolDescriptions::new()).unwrap();
        // Each raw description is valid, but escaped JSON can exceed 128 KiB.
        saved.descriptions = (0..16)
            .map(|n| (format!("tool{n}"), "\\".repeat(8192)))
            .collect();
        saved.hash = saved.canonical_hash().unwrap();
        assert!(saved.validate().is_err());
        saved.descriptions.clear();
        saved.profile = Some("p".repeat(MAX_SNAPSHOT_BYTES));
        saved.hash = saved.canonical_hash().unwrap();
        assert!(saved.validate().is_err());
    }
    #[test]
    fn secret_material_is_refused_without_echoing_the_value() {
        // Assemble a synthetic credential; never include its value in diagnostics.
        let value = ["sk", "-", &"a".repeat(48)].concat();
        let result = snapshot([("read".into(), value)].into());
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .ends_with("snapshot contains secret material")
        );
    }
    #[test]
    fn snapshot_json_limit_is_inclusive_and_counts_metadata_and_escaping() {
        let model = Routed {
            profile: "p",
            model: "m",
        };
        let settings = [(
            "p".into(),
            [("m".into(), [("read".into(), "a".repeat(8192))].into())].into(),
        )]
        .into();
        let mut snapshot = FrozenToolDescriptions::select(
            &settings,
            &model,
            &Default::default(),
            Role::Worker,
            ModelFamily::Gpt5,
            &["read".into()].into(),
        )
        .unwrap();
        // Metadata is counted too; grow the route identity to the exact JSON cap.
        let len = serde_json::to_vec(&snapshot).unwrap().len();
        snapshot
            .route_model_id
            .as_mut()
            .unwrap()
            .push_str(&"a".repeat(MAX_SNAPSHOT_BYTES - len));
        snapshot.hash = snapshot.canonical_hash().unwrap();
        assert_eq!(
            serde_json::to_vec(&snapshot).unwrap().len(),
            MAX_SNAPSHOT_BYTES
        );
        snapshot.validate().unwrap();
        snapshot.route_model_id.as_mut().unwrap().push('a');
        snapshot.hash = snapshot.canonical_hash().unwrap();
        assert!(snapshot.validate().is_err());
    }
}
