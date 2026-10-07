//! 共有可能なプロジェクトルール読み込み元。

use std::path::PathBuf;

use super::types::{ProjectTrust, RulesSettings};

/// 複数 run で共有する不変のルール読み込み設定。
#[derive(Debug, Clone)]
pub struct RulesSource {
    pub(crate) trust: ProjectTrust,
    pub(crate) settings: RulesSettings,
    pub(crate) user_rules_dir: Option<PathBuf>,
    pub(crate) project_root: Option<PathBuf>,
    pub(crate) user_agents_md: Option<PathBuf>,
}

impl RulesSource {
    /// 信頼状態・予算・ユーザ規則・プロジェクトルート・ユーザ AGENTS.md から読み込み元を生成する。
    pub const fn new(
        trust: ProjectTrust,
        settings: RulesSettings,
        user_rules_dir: Option<PathBuf>,
        project_root: Option<PathBuf>,
        user_agents_md: Option<PathBuf>,
    ) -> Self {
        Self {
            trust,
            settings,
            user_rules_dir,
            project_root,
            user_agents_md,
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

    /// 他の設定を保ったまま信頼状態だけを差し替えた読み込み元を返す。
    #[must_use]
    pub fn with_trust(&self, trust: ProjectTrust) -> Self {
        Self {
            trust,
            ..self.clone()
        }
    }

    /// 他の設定を保ったままプロジェクトルートだけを差し替えた読み込み元を返す。
    #[must_use]
    pub fn with_project_root(&self, project_root: Option<PathBuf>) -> Self {
        Self {
            project_root,
            ..self.clone()
        }
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
        let source = RulesSource::new(ProjectTrust::Approved, settings, None, None, None);

        assert_eq!(source.trust(), ProjectTrust::Approved);
        assert_eq!(source.settings(), &settings);
    }
}
