/// 秘密値を保持せず、認証情報の取得先だけを表す参照です。
#[derive(Debug, Clone, PartialEq)]
pub enum CredentialRef {
    /// OS キーリングのサービス名とアカウント名による参照。
    Keyring {
        /// キーリングのサービス名。
        service: String,
        /// キーリングのアカウント名。
        account: String,
    },
    /// 環境変数名による参照。
    Env {
        /// 環境変数名。
        var: String,
    },
}

impl From<&config::CredentialRefConfig> for CredentialRef {
    fn from(value: &config::CredentialRefConfig) -> Self {
        match value {
            config::CredentialRefConfig::Keyring { service, account } => Self::Keyring {
                service: service.clone(),
                account: account.clone(),
            },
            config::CredentialRefConfig::Env { var } => Self::Env { var: var.clone() },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::CredentialRef;

    // Given: キーリングおよび環境変数の認証情報参照 / When: Debug 表現を取得して設定型から変換する
    // Then: 秘密値を持たないフィールドだけが現れ、各変異が正しく写像される
    #[test]
    fn credential_ref_debug_contains_no_secret() {
        let keyring = CredentialRef::Keyring {
            service: "evorch".to_string(),
            account: "primary".to_string(),
        };
        let env = CredentialRef::Env {
            var: "EVORCH_API_KEY".to_string(),
        };

        let keyring_debug = format!("{keyring:?}");
        let env_debug = format!("{env:?}");
        assert!(keyring_debug.contains("service"));
        assert!(keyring_debug.contains("account"));
        assert!(env_debug.contains("var"));
        assert!(!keyring_debug.contains("api_key"));
        assert!(!keyring_debug.contains("token"));
        assert!(!keyring_debug.contains("secret"));
        assert!(!env_debug.contains("api_key"));
        assert!(!env_debug.contains("token"));
        assert!(!env_debug.contains("secret"));

        let keyring_config = config::CredentialRefConfig::Keyring {
            service: "evorch".to_string(),
            account: "primary".to_string(),
        };
        let env_config = config::CredentialRefConfig::Env {
            var: "EVORCH_API_KEY".to_string(),
        };
        assert_eq!(CredentialRef::from(&keyring_config), keyring);
        assert_eq!(CredentialRef::from(&env_config), env);
    }
}
