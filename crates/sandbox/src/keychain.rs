//! OS の資格情報サービスを利用するストアです。

use keyring::{Entry, Error};

use crate::{CredentialError, CredentialStore, Secret};

const SERVICE: &str = "evorch";
const PROBE_KEY: &str = "__evorch_probe__";

/// OS の資格情報サービスへ保存するストア。
#[derive(Debug, Clone, Copy, Default)]
pub struct KeyringCredentialStore;

impl KeyringCredentialStore {
    pub fn probe() -> Result<Self, CredentialError> {
        let entry = Entry::new(SERVICE, PROBE_KEY).map_err(keychain_error)?;
        match entry.get_password() {
            Ok(_) | Err(Error::NoEntry) => Ok(Self),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn entry(key: &str) -> Result<Entry, CredentialError> {
        Entry::new(SERVICE, key).map_err(keychain_error)
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        match Self::entry(key)?.get_password() {
            Ok(value) => Ok(Some(Secret::from(value))),
            Err(Error::NoEntry) => Ok(None),
            Err(error) => Err(keychain_error(error)),
        }
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        Self::entry(key)?
            .set_password(value.expose())
            .map_err(keychain_error)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        match Self::entry(key)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(error) => Err(keychain_error(error)),
        }
    }
}

fn keychain_error(error: Error) -> CredentialError {
    CredentialError::KeychainUnavailable {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::*;

    // Given: 資格情報サービスの機能確認結果 / When: 既定ストアを開く / Then: 利用不可ならファイルへ確実に切り替わる
    #[test]
    fn unavailable_service_falls_back_to_file() {
        if KeyringCredentialStore::probe().is_ok() {
            return;
        }
        let dir = tempdir().expect("一時ディレクトリを作成できるはずです");
        let store = crate::open_default(dir.path()).expect("既定ストアを開けるはずです");
        assert!(store.get("missing").expect("取得できるはずです").is_none());
        assert!(dir.path().join("credentials.json").exists());
    }
}
