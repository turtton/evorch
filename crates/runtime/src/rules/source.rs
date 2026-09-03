//! 共有可能なプロジェクトルール読み込み元。

use std::path::PathBuf;

use super::types::{ProjectTrust, RulesSettings};

/// 複数 run で共有する不変のルール読み込み設定。
#[derive(Debug)]
pub struct RulesSource {
    pub(crate) trust: ProjectTrust,
    pub(crate) settings: RulesSettings,
    pub(crate) user_rules_dir: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
}

impl RulesSource {
    /// 信頼状態・予算・ユーザ規則・プロジェクトルートから読み込み元を生成する。
    pub const fn new(
        trust: ProjectTrust,
        settings: RulesSettings,
        user_rules_dir: Option<PathBuf>,
        project_root: Option<PathBuf>,
    ) -> Self {
        Self {
            trust,
            settings,
            user_rules_dir,
            project_root,
        }
    }

    /// プロジェクトの信頼状態を返す。
    pub const fn trust(&self) -> ProjectTrust {
        self.trust
    }

    /// ルール注入の予算設定を返す。
    pub const fn settings(&self) -> &RulesSettings {
        &self.settings
    }

    /// 構成時に指定されたプロジェクトルートを返す。
    pub fn project_root(&self) -> Option<&std::path::Path> {
        self.project_root.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 信頼状態と予算 / When: RulesSource を生成 / Then: accessor が同じ値を返す
    #[test]
    fn source_exposes_immutable_configuration() {
        let settings = RulesSettings {
            context_window_tokens: 10,
            response_headroom_tokens: 2,
            max_injection_bytes: 8,
        };
        let source = RulesSource::new(ProjectTrust::Approved, settings, None, None);

        assert_eq!(source.trust(), ProjectTrust::Approved);
        assert_eq!(source.settings(), &settings);
    }
}
