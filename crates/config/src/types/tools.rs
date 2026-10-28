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
}

impl Default for RunJsConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            default_timeout_ms: 30_000,
            max_timeout_ms: 300_000,
            memory_limit_mib: 64,
            max_tool_calls: 1000,
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
}
