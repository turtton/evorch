//! Structural validation; runtime owns the builtin registry and secret checks.
use crate::{Config, ConfigError};
use std::collections::BTreeMap;

pub type ToolDescriptions = BTreeMap<String, String>;
pub type ModelToolDescriptions = BTreeMap<String, BTreeMap<String, ToolDescriptions>>;
pub const MAX_DESCRIPTION_BYTES: usize = 8 * 1024;
pub const MAX_MODEL_TOOLS: usize = 64;

pub fn validate_map(map: &ToolDescriptions) -> Result<(), ConfigError> {
    if map.len() > MAX_MODEL_TOOLS {
        return Err(invalid("more than 64 tool descriptions"));
    }
    if map.iter().any(|(name, text)| {
        name.trim().is_empty() || text.trim().is_empty() || text.len() > MAX_DESCRIPTION_BYTES
    }) {
        return Err(invalid(
            "tool names and descriptions must be nonempty; descriptions must be at most 8192 UTF-8 bytes",
        ));
    }
    Ok(())
}
fn invalid(reason: &str) -> ConfigError {
    ConfigError::Migration(format!("model_tool_descriptions: {reason}"))
}
impl Config {
    pub fn validate_model_tool_descriptions(&self) -> Result<(), ConfigError> {
        for (profile, models) in &self.model_tool_descriptions {
            let provider = self
                .providers
                .get(profile)
                .filter(|_| !profile.trim().is_empty())
                .ok_or_else(|| invalid("unknown provider profile"))?;
            for (model, descriptions) in models {
                if model.trim().is_empty()
                    || model.chars().any(char::is_control)
                    || (!provider.models.is_empty()
                        && !provider.models.iter().any(|entry| {
                            entry.id == *model
                                || entry.id == crate::types::provider::parse_model_speed(model).0
                        })
                        && provider.default_model != *model)
                {
                    return Err(invalid("unknown or invalid route model ID"));
                }
                validate_map(descriptions)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn description_limits_are_utf8_bytes_and_not_characters() {
        let mut map = ToolDescriptions::from([("read".into(), "é".repeat(4096))]);
        validate_map(&map).unwrap();
        map.get_mut("read").unwrap().push('x');
        assert!(validate_map(&map).is_err());
        map.insert("read".into(), " \n".into());
        assert!(validate_map(&map).is_err());
        validate_map(&ToolDescriptions::new()).unwrap();
        let map = (0..64)
            .map(|n| (format!("tool{n}"), "description".into()))
            .collect();
        validate_map(&map).unwrap();
        let mut map = map;
        map.insert("overflow".into(), "description".into());
        assert!(validate_map(&map).is_err());
    }
    #[test]
    fn profiles_and_route_model_ids_are_exact_including_fast() {
        let mut cfg = Config::default();
        cfg.providers.insert(
            "p".into(),
            crate::ProviderProfileConfig {
                models: ["model", "model+fast"]
                    .map(|id| serde_json::from_value(serde_json::json!({"id": id})).unwrap())
                    .into(),
                ..Default::default()
            },
        );
        cfg.model_tool_descriptions.insert(
            "p".into(),
            BTreeMap::from([("model+fast".into(), ToolDescriptions::new())]),
        );
        cfg.validate_model_tool_descriptions().unwrap();
        cfg.model_tool_descriptions
            .get_mut("p")
            .unwrap()
            .insert("unlisted".into(), ToolDescriptions::new());
        assert!(cfg.validate_model_tool_descriptions().is_err());
        cfg.model_tool_descriptions.clear();
        cfg.model_tool_descriptions
            .insert("missing".into(), BTreeMap::new());
        assert!(cfg.validate_model_tool_descriptions().is_err());
    }
    #[test]
    fn strict_loader_preserves_exact_description_maps_and_project_cannot_inject_them() {
        let user = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            user.path().join("config.toml"),
            r#"
version = 3
[providers.p]
type = "openai-compatible"
base_url = "http://127.0.0.1:1"
api_key_env = "OFFLINE_KEY"
models = ["model", "model+fast"]
default_model = "model"
[model_tool_descriptions.p."model+fast"]
read = "Exact fast description"
"#,
        )
        .unwrap();
        std::fs::create_dir_all(project.path().join(crate::PROJECT_CONFIG_DIR)).unwrap();
        std::fs::write(
            crate::project_main_config_path(project.path()),
            r#"
[model_tool_descriptions.p.model]
read = "Untrusted project injection"
"#,
        )
        .unwrap();
        let loaded = Config::load_strict(&crate::LoadOptions {
            user_config_dir: Some(user.path().into()),
            project_dir: Some(project.path().into()),
            read_env: false,
            ..Default::default()
        })
        .unwrap();
        assert_eq!(
            loaded.model_tool_descriptions["p"]["model+fast"]["read"],
            "Exact fast description"
        );
        assert!(!loaded.model_tool_descriptions["p"].contains_key("model"));
    }
}
