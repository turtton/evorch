//! Settings for built-in tools. Only defaults and user/global files may set
//! them: `browser.executable` names a program to run, and the rest widens what
//! agents can do.

use std::num::NonZeroU32;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub browser: BrowserToolsConfig,
    pub run_js: RunJsConfig,
}

/// The `run_js` code-mode tool (ADR 0039). Off by default: the tool is offered
/// only when `enabled` is set, so default requests keep their tool list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RunJsConfig {
    /// Offers `run_js` to the roles that may use it.
    pub enabled: bool,
    /// Time limit of one call when it passes no `timeout_ms`. Waiting for the
    /// tool calls made inside the code counts.
    pub default_timeout_ms: u64,
    /// Largest `timeout_ms` a call may ask for.
    pub max_timeout_ms: u64,
    /// Memory limit of the JavaScript engine, in MiB.
    pub memory_limit_mib: u32,
    /// Tool calls the code may make in one `run_js` call. A page of a file that a
    /// result offered as its `next` (how `readText` assembles a longer file) does not
    /// count the first time the code asks for it.
    pub max_tool_calls: u32,
    /// Where `run_js` is offered, when `enabled`. Each entry is a role
    /// (`"worker"`: every category of it and runs without one) or a role with
    /// one category (`"worker:deep"`), spelled as in `agents.<role>` and
    /// `agents.<role>.categories.<name>`. A role without the `run_js`
    /// capability never gets it, whatever is listed.
    pub scopes: Vec<String>,
}

/// Roles a `run_js` scope may name, spelled as their `agents.<role>` keys. All
/// runtime roles are accepted; the ones without the `run_js` capability
/// (orchestrator, multimodal_looker, evaluator) simply never get the tool.
pub const RUN_JS_SCOPE_ROLES: &[&str] = &[
    "orchestrator",
    "explorer",
    "worker",
    "reviewer",
    "web_researcher",
    "planner",
    "advisor",
    "multimodal_looker",
    "evaluator",
];

/// Scopes of the default config: the strongest Worker categories only.
pub const DEFAULT_RUN_JS_SCOPES: &[&str] = &["worker:deep", "worker:high-reasoning"];

/// Checks one `scopes` entry: a known role, optionally with `:<category>` that
/// is registered for that role (the rule of `agents.<role>.categories`).
///
/// # Errors
/// Returns what is wrong with `scope`.
pub fn validate_run_js_scope(scope: &str) -> Result<(), String> {
    let (role, category) = match scope.split_once(':') {
        Some((role, category)) => (role, Some(category)),
        None => (scope, None),
    };
    if !RUN_JS_SCOPE_ROLES.contains(&role) {
        return Err(format!(
            "unknown role `{role}` in scope `{scope}`, expected one of: {}",
            RUN_JS_SCOPE_ROLES.join(", ")
        ));
    }
    let Some(category) = category else {
        return Ok(());
    };
    if crate::agent_categories::category_for_role(role, category).is_some() {
        return Ok(());
    }
    let known = crate::agent_categories::categories_for_role(role)
        .map(|category| category.id.as_str())
        .collect::<Vec<_>>();
    Err(if known.is_empty() {
        format!("role `{role}` has no categories, so `{scope}` is not allowed; use `{role}`")
    } else {
        format!(
            "unknown category `{category}` for role `{role}` in scope `{scope}`, expected one of: {}",
            known.join(", ")
        )
    })
}

impl RunJsConfig {
    /// Whether `run_js` is switched on and `scopes` name a run of `role` (its
    /// `agents.<role>` key) with `category`. Whether the role may use the tool
    /// at all is its capability, checked by the runtime.
    pub fn offered_to(&self, role: &str, category: Option<&str>) -> bool {
        self.enabled
            && self.scopes.iter().any(|scope| match scope.split_once(':') {
                None => scope == role,
                Some((scope_role, scope_category)) => {
                    scope_role == role && category == Some(scope_category)
                }
            })
    }
}

impl Default for RunJsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_ms: 30_000,
            max_timeout_ms: 300_000,
            memory_limit_mib: 64,
            max_tool_calls: 1000,
            scopes: DEFAULT_RUN_JS_SCOPES
                .iter()
                .map(|scope| (*scope).to_owned())
                .collect(),
        }
    }
}

/// The agent browser tools (ADR 0034).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserToolsConfig {
    /// Registers the tools when a Chromium or Chrome executable is found.
    pub enabled: bool,
    /// The browser to launch. Unset uses `CHROME`, then auto-detection.
    pub executable: Option<PathBuf>,
    /// Agent browser tabs kept open across all runs. Going over closes every
    /// tab of the least recently used other run.
    pub max_pages: NonZeroU32,
    /// Tabs one run may keep open, counting popups its pages open; capped by
    /// `max_pages`. Popups beyond it are closed as soon as they open.
    pub max_tabs: NonZeroU32,
    /// Also registers `browser_click_at`, which clicks viewport coordinates
    /// read from a screenshot rather than an outline ref.
    pub coordinate_actions: bool,
}

impl Default for BrowserToolsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executable: None,
            max_pages: NonZeroU32::new(8).expect("8 is non-zero"),
            max_tabs: NonZeroU32::new(4).expect("4 is non-zero"),
            coordinate_actions: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn browser_tools_default_on_without_coordinates_and_parse_partially() {
        let default = BrowserToolsConfig::default();
        assert_eq!(Config::default().tools.browser, default);
        assert_eq!(toml::from_str::<Config>("").unwrap().tools.browser, default);

        let parsed: Config = toml::from_str(
            "[tools.browser]\nexecutable = \"/opt/chromium\"\ncoordinate_actions = true",
        )
        .unwrap();

        assert_eq!(
            parsed.tools.browser,
            BrowserToolsConfig {
                executable: Some("/opt/chromium".into()),
                coordinate_actions: true,
                ..default
            }
        );
        assert!(toml::from_str::<Config>("[tools.browser]\nmax_pages = 0").is_err());
        assert!(toml::from_str::<Config>("[tools.browser]\nmax_tabs = 0").is_err());
    }

    #[test]
    fn run_js_defaults_off_with_documented_limits_and_parses_partially() {
        let default = RunJsConfig::default();
        assert!(!default.enabled);
        assert_eq!(
            (
                default.default_timeout_ms,
                default.max_timeout_ms,
                default.memory_limit_mib,
                default.max_tool_calls
            ),
            (30_000, 300_000, 64, 1000)
        );
        assert_eq!(Config::default().tools.run_js, default);
        assert_eq!(toml::from_str::<Config>("").unwrap().tools.run_js, default);

        let parsed: Config =
            toml::from_str("[tools.run_js]\nenabled = true\nmax_tool_calls = 10").unwrap();

        assert_eq!(
            parsed.tools.run_js,
            RunJsConfig {
                enabled: true,
                max_tool_calls: 10,
                ..default
            }
        );
        assert!(toml::from_str::<Config>("[tools.run_js]\nunknown = 1").is_err());
    }

    #[test]
    fn run_js_scopes_default_to_the_strong_worker_categories() {
        let config = RunJsConfig {
            enabled: true,
            ..RunJsConfig::default()
        };
        assert_eq!(config.scopes, ["worker:deep", "worker:high-reasoning"]);
        assert!(config.offered_to("worker", Some("deep")));
        assert!(config.offered_to("worker", Some("high-reasoning")));
        assert!(!config.offered_to("worker", None));
        assert!(!config.offered_to("worker", Some("quick")));
        assert!(!config.offered_to("explorer", None));
        assert!(!config.offered_to("reviewer", Some("plan-review")));
    }

    #[test]
    fn a_role_scope_covers_every_category_and_none_while_a_pair_covers_one() {
        let parse = |scopes: &str| {
            toml::from_str::<Config>(&format!(
                "[tools.run_js]\nenabled = true\nscopes = {scopes}"
            ))
            .unwrap()
            .tools
            .run_js
        };

        let role = parse("[\"worker\", \"reviewer:plan-review\"]");
        assert!(role.offered_to("worker", None));
        assert!(role.offered_to("worker", Some("quick")));
        assert!(role.offered_to("reviewer", Some("plan-review")));
        assert!(!role.offered_to("reviewer", None));
        assert!(!role.offered_to("reviewer", Some("tool-execution")));
        assert!(!role.offered_to("explorer", None));

        assert!(!parse("[]").offered_to("worker", Some("deep")));
        let off = RunJsConfig {
            enabled: false,
            ..role
        };
        assert!(!off.offered_to("worker", None));
    }

    #[test]
    fn scope_validation_names_unknown_roles_and_categories() {
        for valid in [
            "worker",
            "worker:deep",
            "reviewer:plan-review",
            "web_researcher",
            "orchestrator",
        ] {
            assert_eq!(validate_run_js_scope(valid), Ok(()), "{valid}");
        }
        for (invalid, expected) in [
            ("", "unknown role ``"),
            ("wroker", "unknown role `wroker`"),
            ("Worker", "unknown role `Worker`"),
            ("worker:", "unknown category ``"),
            ("worker:nope", "unknown category `nope` for role `worker`"),
            ("worker:plan-review", "unknown category `plan-review`"),
            ("explorer:deep", "role `explorer` has no categories"),
        ] {
            let error = validate_run_js_scope(invalid).unwrap_err();
            assert!(error.contains(expected), "{invalid}: {error}");
        }
    }
}
