//! サンドボックスと資格情報のエラーを定義します。

/// サンドボックス構成または起動時のエラー。
#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    /// bubblewrap を安全に利用できない。
    #[error("サンドボックスを利用できません: {detail}")]
    BwrapUnavailable { detail: String },
    /// コマンド仕様が不正である。
    #[error("サンドボックス仕様が不正です: {detail}")]
    InvalidSpec { detail: String },
}

/// 資格情報ストアのエラー。
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// ファイル入出力に失敗した。
    #[error("資格情報の入出力に失敗しました: {detail}")]
    Io { detail: String },
    /// 永続化された資格情報を解析できない。
    #[error("資格情報ファイルが壊れています: {detail}")]
    Malformed { detail: String },
    /// OS の資格情報サービスを利用できない。
    #[error("資格情報サービスを利用できません: {detail}")]
    KeychainUnavailable { detail: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: bubblewrap の検出失敗 / When: エラーを表示 / Then: 利用不可の理由が日本語で示される
    #[test]
    fn sandbox_error_displays_unavailable_detail() {
        let error = SandboxError::BwrapUnavailable {
            detail: "実行ファイルがありません".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "サンドボックスを利用できません: 実行ファイルがありません"
        );
    }

    // Given: 壊れた資格情報ファイル / When: エラーを表示 / Then: 破損理由が日本語で示される
    #[test]
    fn credential_error_displays_malformed_detail() {
        let error = CredentialError::Malformed {
            detail: "JSON が不正です".to_owned(),
        };

        assert_eq!(
            error.to_string(),
            "資格情報ファイルが壊れています: JSON が不正です"
        );
    }
}
