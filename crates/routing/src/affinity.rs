//! セッションアフィニティ (チェーン内のメンバーのピン留め) を管理します。

use std::collections::BTreeMap;

/// チェーン内でピンしたメンバー。
///
/// `position` は展開したチェーン ([`crate::Chain`]) 上の位置で、設定が変わって
/// 位置がずれた場合は (プロファイル, 具体モデル) で引き直します。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct RoutePin {
    /// 展開したチェーン上の位置。
    pub position: usize,
    /// プロファイル名。
    pub profile: String,
    /// 具体モデル ID。
    pub model: String,
}

/// セッションごとに、チェーンからピンしたメンバーへの対応を管理します。
///
/// 同一セッション内で一度選択されたメンバーをチェーン単位で固定し、
/// 以降の解決でも同じメンバーを使います (ADR 0030: ピンは元に戻さない)。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionAffinity {
    /// セッション ID → (チェーンのキー → ピン)。
    sessions: BTreeMap<String, BTreeMap<String, RoutePin>>,
}

impl SessionAffinity {
    /// セッションのチェーンを指定したメンバーにピンします。既存のピンは上書きします。
    pub fn pin(&mut self, session_id: &str, chain: &str, pin: RoutePin) {
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .insert(chain.to_string(), pin);
    }

    /// セッションのチェーンのピンを解除します。ピンがなければ何もしません。
    pub fn forget(&mut self, session_id: &str, chain: &str) {
        if let Some(pinned) = self.sessions.get_mut(session_id) {
            pinned.remove(chain);
        }
    }

    /// セッションのチェーンのピンを返します。
    pub fn pinned(&self, session_id: &str, chain: &str) -> Option<&RoutePin> {
        self.sessions
            .get(session_id)
            .and_then(|pinned| pinned.get(chain))
    }

    /// ピンを持つセッション ID を昇順に返します (再構成時の引き継ぎ用)。
    pub fn sessions(&self) -> Vec<String> {
        self.sessions
            .iter()
            .filter(|(_, pinned)| !pinned.is_empty())
            .map(|(session, _)| session.clone())
            .collect()
    }

    /// セッションの全ピンをチェーンのキー順に返します (スナップショット保存用)。
    pub fn pins(&self, session_id: &str) -> Vec<(String, RoutePin)> {
        self.sessions
            .get(session_id)
            .map(|pinned| {
                pinned
                    .iter()
                    .map(|(chain, pin)| (chain.clone(), pin.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::{RoutePin, SessionAffinity};

    fn pin(position: usize, profile: &str, model: &str) -> RoutePin {
        RoutePin {
            position,
            profile: profile.into(),
            model: model.into(),
        }
    }

    // Given: 何もピンしていないアフィニティ
    // When: 同一セッションに 2 つのチェーンをピンする
    // Then: それぞれのピンを参照でき、別セッションは影響を受けない
    #[test]
    fn pin_then_pinned_returns_member() {
        let mut affinity = SessionAffinity::default();

        affinity.pin("session-1", "worker", pin(0, "primary", "model-a"));
        affinity.pin("session-1", "reviewer", pin(2, "secondary", "model-b"));

        assert_eq!(
            affinity.pinned("session-1", "worker"),
            Some(&pin(0, "primary", "model-a"))
        );
        assert_eq!(
            affinity.pinned("session-1", "reviewer"),
            Some(&pin(2, "secondary", "model-b"))
        );
        assert_eq!(affinity.pinned("session-2", "worker"), None);
        assert_eq!(affinity.pinned("session-1", "missing"), None);
        assert_eq!(affinity.sessions(), ["session-1"]);
        assert_eq!(
            affinity.pins("session-1"),
            [
                ("reviewer".to_owned(), pin(2, "secondary", "model-b")),
                ("worker".to_owned(), pin(0, "primary", "model-a")),
            ]
        );
    }

    // Given: ピン済みのアフィニティ / When: 別メンバーをピンし直す・解除する
    // Then: 上書きされ、解除後は参照できない。存在しない対象の解除は何もしない
    #[test]
    fn pin_overwrites_and_forget_removes() {
        let mut affinity = SessionAffinity::default();
        affinity.pin("session-1", "worker", pin(0, "primary", "model-a"));

        affinity.pin("session-1", "worker", pin(1, "secondary", "model-b"));
        assert_eq!(
            affinity.pinned("session-1", "worker"),
            Some(&pin(1, "secondary", "model-b"))
        );

        affinity.forget("session-1", "worker");
        affinity.forget("missing-session", "worker");
        assert_eq!(affinity.pinned("session-1", "worker"), None);
        assert!(affinity.pins("missing-session").is_empty());
    }
}
