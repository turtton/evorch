//! 編集成功後に実行する外部 comment-checker の設定。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CommentCheckerConfig {
    pub enabled: bool,
    /// 裸名はホストの PATH から探索し、`/` を含む場合は明示パスとして扱う。
    pub binary: String,
    pub timeout_ms: u64,
    pub prompt: Option<String>,
}

impl Default for CommentCheckerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            binary: "comment-checker".into(),
            timeout_ms: 15_000,
            prompt: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Config;

    #[test]
    fn defaults_and_partial_sections_preserve_checker_defaults() {
        let default = Config::default().comment_checker;
        assert!(default.enabled);
        assert_eq!(default.binary, "comment-checker");
        assert_eq!(default.timeout_ms, 15_000);
        assert_eq!(default.prompt, None);
        assert_eq!(
            toml::from_str::<Config>("").unwrap().comment_checker,
            default
        );
        let parsed: Config = toml::from_str("[comment_checker]\nenabled = false").unwrap();
        assert_eq!(
            parsed.comment_checker,
            CommentCheckerConfig {
                enabled: false,
                ..default
            }
        );
    }

    #[test]
    fn checker_settings_parse_and_unknown_fields_are_rejected() {
        let parsed: Config = toml::from_str(
            r#"
[comment_checker]
enabled = true
binary = "/opt/bin/checker"
timeout_ms = 42
prompt = "Review {{comments}}"
"#,
        )
        .unwrap();
        assert_eq!(
            parsed.comment_checker,
            CommentCheckerConfig {
                enabled: true,
                binary: "/opt/bin/checker".into(),
                timeout_ms: 42,
                prompt: Some("Review {{comments}}".into()),
            }
        );
        assert!(toml::from_str::<Config>("[comment_checker]\nextra_args = []").is_err());
    }
}
