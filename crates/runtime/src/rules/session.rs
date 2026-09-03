//! run ごとのプロジェクトルール状態。

use std::path::PathBuf;
use std::sync::Arc;

use providers::Usage;

use super::source::RulesSource;

/// 単一 run が保持するルール解決状態。
#[derive(Debug)]
pub struct RulesSession {
    pub(crate) source: Arc<RulesSource>,
    pub(crate) active_root: Option<PathBuf>,
    pub(crate) last_usage: Option<Usage>,
}

impl RulesSession {
    /// 共有設定と run の有効ルートからセッションを生成する。
    pub fn new(source: Arc<RulesSource>, active_root: Option<PathBuf>) -> Self {
        Self {
            source,
            active_root,
            last_usage: None,
        }
    }

    /// 最新のプロバイダ使用量を更新する。
    pub const fn set_last_usage(&mut self, usage: Usage) {
        self.last_usage = Some(usage);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules::types::{ProjectTrust, RulesSettings};

    // Given: 新規セッション / When: 使用量を更新 / Then: 最新値が保持される
    #[test]
    fn session_retains_latest_usage() {
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Unapproved,
            RulesSettings {
                context_window_tokens: 10,
                response_headroom_tokens: 1,
                max_injection_bytes: 10,
            },
            None,
            None,
        ));
        let mut session = RulesSession::new(source, None);
        let usage = Usage {
            input_tokens: 3,
            output_tokens: 2,
            ..Usage::default()
        };

        session.set_last_usage(usage);

        assert_eq!(session.last_usage, Some(usage));
    }
}
