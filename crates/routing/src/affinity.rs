//! セッションアフィニティ (プロバイダのピン留め) を管理します。

use std::collections::BTreeMap;

/// セッションごとに、論理モデルからプロバイダプロファイルへのピンを管理します。
///
/// 同一セッション内で一度選択されたプロバイダプロファイルを論理モデル単位で
/// 固定しておき、以降の解決で同じプロファイルを使い続けることを可能にします。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SessionAffinity {
    /// セッション ID → (論理モデル名 → プロファイル名)。
    sessions: BTreeMap<String, BTreeMap<String, String>>,
}

impl SessionAffinity {
    /// セッションの論理モデルを指定したプロバイダプロファイルにピンします。
    ///
    /// 同じセッション・論理モデルの組み合わせが既にピンされている場合は
    /// 上書きします。
    pub fn pin(&mut self, session_id: &str, logical: &str, profile: &str) {
        self.sessions
            .entry(session_id.to_string())
            .or_default()
            .insert(logical.to_string(), profile.to_string());
    }

    /// セッションの論理モデルのピンを解除します。
    ///
    /// ピンが存在しない場合は何もしません。
    pub fn forget(&mut self, session_id: &str, logical: &str) {
        if let Some(pinned) = self.sessions.get_mut(session_id) {
            pinned.remove(logical);
        }
    }

    /// セッションの論理モデルがピンしているプロバイダプロファイル名を返します。
    ///
    /// ピンされていない場合は `None` を返します。
    pub fn pinned(&self, session_id: &str, logical: &str) -> Option<&str> {
        self.sessions
            .get(session_id)
            .and_then(|pinned| pinned.get(logical))
            .map(String::as_str)
    }
}

#[cfg(test)]
mod tests {
    use super::SessionAffinity;

    // Given: 何もピンしていないアフィニティ
    // When: 同一セッションに 2 つの論理モデルをピンする
    // Then: それぞれのプロファイル名を参照でき、別セッションは影響を受けない
    #[test]
    fn pin_then_pinned_returns_profile_name() {
        let mut affinity = SessionAffinity::default();

        affinity.pin("session-1", "summary", "primary");
        affinity.pin("session-1", "review", "secondary");

        assert_eq!(affinity.pinned("session-1", "summary"), Some("primary"));
        assert_eq!(affinity.pinned("session-1", "review"), Some("secondary"));
        assert_eq!(
            affinity.pinned("session-2", "summary"),
            None,
            "セッションは互いに独立する"
        );
        assert_eq!(
            affinity.pinned("session-1", "missing"),
            None,
            "未ピンの論理モデルは None"
        );
    }

    // Given: 同じセッション・論理モデルにピン済みのアフィニティ
    // When: 別のプロファイル名をピンし直す
    // Then: 後のピンで上書きされる
    #[test]
    fn pin_overwrites_existing_pin() {
        let mut affinity = SessionAffinity::default();
        affinity.pin("session-1", "summary", "primary");

        affinity.pin("session-1", "summary", "secondary");

        assert_eq!(affinity.pinned("session-1", "summary"), Some("secondary"));
    }

    // Given: ピン済みのアフィニティ
    // When: forget でピンを解除する / 存在しないセッション・論理モデルに forget する
    // Then: ピンが消え、存在しない対象への forget は何も起こさない
    #[test]
    fn forget_removes_pin_and_is_noop_when_absent() {
        let mut affinity = SessionAffinity::default();
        affinity.pin("session-1", "summary", "primary");

        affinity.forget("session-1", "summary");
        assert_eq!(
            affinity.pinned("session-1", "summary"),
            None,
            "解除済みのピンは参照できない"
        );

        affinity.forget("session-1", "summary");
        affinity.forget("missing-session", "summary");
        assert_eq!(
            affinity.pinned("missing-session", "summary"),
            None,
            "存在しない対象への forget は何もしない"
        );
    }
}
