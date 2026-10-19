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
}

/// The agent browser tools (ADR 0034).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct BrowserToolsConfig {
    /// Registers the tools when a Chromium or Chrome executable is found.
    pub enabled: bool,
    /// The browser to launch. Unset uses `CHROME`, then auto-detection.
    pub executable: Option<PathBuf>,
    /// Agent pages kept open; opening another closes the least recently used.
    pub max_pages: NonZeroU32,
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
    }
}
