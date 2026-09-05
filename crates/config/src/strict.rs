//! マージ済み設定の許可フィールドを厳格に検査します。
//!
//! ADR 0014 の HARD contract として、未知キーによる typo の黙殺と平文 credential の
//! 混入を、型へのデシリアライズより前に完全な config path 付きで拒否します。

use crate::ConfigError;
use crate::types::agents::CATEGORY_NAMES;

const ROOT_KEYS: &[&str] = &[
    "version",
    "providers",
    "routing",
    "panel",
    "diagnostics",
    "permissions",
    "metrics",
    "agents",
    "rules",
    "compaction",
    "orchestration",
];
const PROVIDER_KEYS: &[&str] = &[
    "provider_type",
    "type",
    "api_protocol",
    "base_url",
    "credential",
    "api_key_env",
    "models",
    "default_model",
];
const KEYRING_KEYS: &[&str] = &["type", "service", "account"];
const ENV_KEYS: &[&str] = &["type", "var"];
const ROUTING_KEYS: &[&str] = &["routes"];
const ROUTE_CANDIDATE_KEYS: &[&str] = &["profile", "model"];
const PANEL_KEYS: &[&str] = &["layout", "keybinds"];
const DIAGNOSTICS_KEYS: &[&str] = &["log_level", "log_dir"];
const PERMISSIONS_KEYS: &[&str] = &["preset"];
const METRICS_KEYS: &[&str] = &["enabled", "retention_days"];
const AGENTS_KEYS: &[&str] = &["orchestrator", "explorer", "worker", "reviewer"];
const RULES_KEYS: &[&str] = &[
    "context_window_tokens",
    "response_headroom_tokens",
    "max_injection_bytes",
];
const COMPACTION_KEYS: &[&str] = &[
    "enabled",
    "threshold",
    "context_window_tokens",
    "model_overrides",
    "keep_recent_tokens",
    "cooldown_turns",
    "max_compactions_per_run",
    "max_summary_bytes",
    "summarizer",
];
const ORCHESTRATION_KEYS: &[&str] = &[
    "max_review_rounds",
    "max_nudges",
    "stall_after_secs",
    "stall_check_secs",
    "in_flight_tool_multiplier",
    "repeated_error_threshold",
    "max_continuations",
    "ci_poll_secs",
    "ci_timeout_secs",
];
const ROLE_BINDING_KEYS: &[&str] = &["logical_model", "preset", "generation", "categories"];
const CATEGORY_BINDING_KEYS: &[&str] = &["logical_model", "preset", "generation"];
const GENERATION_KEYS: &[&str] = &["temperature", "top_p", "max_tokens", "reasoning_effort"];
const CREDENTIAL_LIKE_KEYS: &[&str] = &[
    "api_key",
    "apikey",
    "api_token",
    "access_token",
    "auth_token",
    "refresh_token",
    "token",
    "secret",
    "client_secret",
    "secret_key",
    "api_secret",
    "password",
    "passphrase",
    "credential_value",
    "credentials",
    "private_key",
    "bearer_token",
];
const CREDENTIAL_MESSAGE: &str = "plaintext credential value must not be stored in config (ADR 0014); use the credential reference instead: credential = { type = \"keyring\", service = \"evorch\", account = \"...\" } or credential = { type = \"env\", var = \"...\" }";

/// マージ済み設定に未知キーまたは平文 credential がないことを検査する。
///
/// # Errors
/// 最初に見つかった違反を [`ConfigError::InvalidField`] として返す。
pub(crate) fn validate_strict(merged: &toml::Value) -> Result<(), ConfigError> {
    let Some(root) = merged.as_table() else {
        return Ok(());
    };
    check_keys(root, "", ROOT_KEYS)?;

    if let Some(providers) = root.get("providers").and_then(toml::Value::as_table) {
        for (name, value) in providers {
            let Some(profile) = value.as_table() else {
                continue;
            };
            let profile_path = format!("providers.{name}");
            check_credential_scope_keys(profile, &profile_path, PROVIDER_KEYS)?;
            validate_api_key_env(profile, &profile_path)?;
            validate_credential(profile, &profile_path)?;
        }
    }

    validate_routing(root)?;
    validate_agents(root)?;
    validate_section(root, "panel", PANEL_KEYS)?;
    validate_section(root, "diagnostics", DIAGNOSTICS_KEYS)?;
    validate_section(root, "permissions", PERMISSIONS_KEYS)?;
    validate_section(root, "metrics", METRICS_KEYS)?;
    validate_section(root, "rules", RULES_KEYS)?;
    validate_section(root, "compaction", COMPACTION_KEYS)?;
    validate_section(root, "orchestration", ORCHESTRATION_KEYS)
}

// api_key_env は credential と併用できず、空でない文字列でなければならない。
// デシリアライズ前に生テーブルで検査することで、エラーに完全な config path を載せる。
fn validate_api_key_env(
    profile: &toml::value::Table,
    profile_path: &str,
) -> Result<(), ConfigError> {
    let Some(api_key_env) = profile.get("api_key_env") else {
        return Ok(());
    };
    let path = format!("{profile_path}.api_key_env");
    if profile.contains_key("credential") {
        return Err(ConfigError::InvalidField {
            path,
            message: "mutually exclusive with `credential`; use one or the other".to_string(),
        });
    }
    match api_key_env.as_str() {
        Some(var) if !var.trim().is_empty() => Ok(()),
        _ => Err(ConfigError::InvalidField {
            path,
            message: "must be a non-empty environment variable name; \
                      use credential = { type = \"env\", var = \"...\" } for a reference"
                .to_string(),
        }),
    }
}

fn validate_credential(
    profile: &toml::value::Table,
    profile_path: &str,
) -> Result<(), ConfigError> {
    let Some(credential) = profile.get("credential").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    let Some(credential_type) = credential.get("type").and_then(toml::Value::as_str) else {
        return Ok(());
    };
    let credential_path = format!("{profile_path}.credential");
    match credential_type {
        "keyring" => check_credential_scope_keys(credential, &credential_path, KEYRING_KEYS),
        "env" => check_credential_scope_keys(credential, &credential_path, ENV_KEYS),
        _ => Ok(()),
    }
}

fn validate_routing(root: &toml::value::Table) -> Result<(), ConfigError> {
    let Some(routing) = root.get("routing").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    check_keys(routing, "routing", ROUTING_KEYS)?;
    let Some(routes) = routing.get("routes").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (route, value) in routes {
        let Some(candidates) = value.as_array() else {
            continue;
        };
        for (index, candidate) in candidates.iter().enumerate() {
            let Some(candidate) = candidate.as_table() else {
                continue;
            };
            check_keys(
                candidate,
                &format!("routing.routes.{route}[{index}]"),
                ROUTE_CANDIDATE_KEYS,
            )?;
        }
    }
    Ok(())
}

fn validate_agents(root: &toml::value::Table) -> Result<(), ConfigError> {
    let Some(agents) = root.get("agents").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    check_keys(agents, "agents", AGENTS_KEYS)?;
    for (role, value) in agents {
        let Some(binding) = value.as_table() else {
            continue;
        };
        let role_path = format!("agents.{role}");
        check_keys(binding, &role_path, ROLE_BINDING_KEYS)?;
        if let Some(generation) = binding.get("generation").and_then(toml::Value::as_table) {
            check_keys(
                generation,
                &format!("{role_path}.generation"),
                GENERATION_KEYS,
            )?;
        }
        validate_role_categories(binding, &role_path)?;
    }
    Ok(())
}

fn validate_role_categories(
    binding: &toml::value::Table,
    role_path: &str,
) -> Result<(), ConfigError> {
    let Some(categories) = binding.get("categories").and_then(toml::Value::as_table) else {
        return Ok(());
    };
    for (category, value) in categories {
        let category_path = format!("{role_path}.categories.{category}");
        if !CATEGORY_NAMES.contains(&category.as_str()) {
            return Err(ConfigError::InvalidField {
                path: category_path,
                message: format!(
                    "unknown category, expected one of: {}",
                    CATEGORY_NAMES.join(", ")
                ),
            });
        }
        let Some(category_binding) = value.as_table() else {
            continue;
        };
        check_keys(category_binding, &category_path, CATEGORY_BINDING_KEYS)?;
        if let Some(generation) = category_binding
            .get("generation")
            .and_then(toml::Value::as_table)
        {
            check_keys(
                generation,
                &format!("{category_path}.generation"),
                GENERATION_KEYS,
            )?;
        }
    }
    Ok(())
}

fn validate_section(
    root: &toml::value::Table,
    section: &str,
    allowed: &[&str],
) -> Result<(), ConfigError> {
    let Some(table) = root.get(section).and_then(toml::Value::as_table) else {
        return Ok(());
    };
    check_keys(table, section, allowed)
}

fn check_keys(
    table: &toml::value::Table,
    prefix: &str,
    allowed: &[&str],
) -> Result<(), ConfigError> {
    for key in table.keys() {
        if allowed.contains(&key.as_str()) {
            continue;
        }
        return Err(ConfigError::InvalidField {
            path: field_path(prefix, key),
            message: format!("unknown field, expected one of: {}", allowed.join(", ")),
        });
    }
    Ok(())
}

fn check_credential_scope_keys(
    table: &toml::value::Table,
    prefix: &str,
    allowed: &[&str],
) -> Result<(), ConfigError> {
    if let Some(key) = table
        .keys()
        .find(|key| !allowed.contains(&key.as_str()) && is_credential_like(key))
    {
        return Err(ConfigError::InvalidField {
            path: field_path(prefix, key),
            message: CREDENTIAL_MESSAGE.to_string(),
        });
    }
    check_keys(table, prefix, allowed)
}

fn field_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

fn is_credential_like(key: &str) -> bool {
    let normalized = key.to_ascii_lowercase().replace('-', "_");
    CREDENTIAL_LIKE_KEYS.contains(&normalized.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_like_normalization_matches_ascii_case_and_hyphen() {
        // Given: 大文字とハイフンを含む credential-like key
        // When: denylist と照合する
        // Then: 正規化後の api_key として検出される
        assert!(is_credential_like("API-KEY"));
    }

    #[test]
    fn credential_field_itself_is_not_credential_like() {
        // Given: 安全な参照オブジェクトを格納する credential key
        // When: denylist と照合する
        // Then: 平文 credential-like key とは判定されない
        assert!(!is_credential_like("credential"));
    }

    #[test]
    fn routing_candidate_error_contains_indexed_path() {
        // Given: routing candidate に未知の weight key があるマージ済み設定
        let merged: toml::Value =
            toml::from_str("[[routing.routes.fast]]\nprofile = \"p\"\nweight = 3\n")
                .expect("routing 設定を解析できる");

        // When: strict validation を実行する
        let error = validate_strict(&merged).expect_err("未知キーは拒否される");

        // Then: route 名と候補 index を含む完全な path になる
        assert!(error.to_string().contains("routing.routes.fast[0].weight"));
    }
}
