//! 資格情報の秘匿値と永続化ストアを提供します。

use std::{
    collections::HashMap,
    fmt, fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use tempfile::NamedTempFile;

use crate::error::CredentialError;

/// ログやデバッグ表示で内容を伏せる秘密値。
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Secret(<redacted>)")
    }
}

/// 資格情報を読み書きする同期ストア。
pub trait CredentialStore: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError>;
    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError>;
    fn delete(&self, key: &str) -> Result<(), CredentialError>;
}

/// 権限を制限した JSON ファイルによる資格情報ストア。
pub struct FileCredentialStore {
    dir: PathBuf,
    path: PathBuf,
    values: RwLock<HashMap<String, String>>,
}

impl FileCredentialStore {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self, CredentialError> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir).map_err(io_error)?;
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).map_err(io_error)?;
        let path = dir.join("credentials.json");
        let values = if path.exists() {
            let bytes = fs::read(&path).map_err(io_error)?;
            serde_json::from_slice(&bytes).map_err(|error| CredentialError::Malformed {
                detail: error.to_string(),
            })?
        } else {
            HashMap::new()
        };
        let store = Self {
            dir,
            path,
            values: RwLock::new(values),
        };
        if !store.path.exists() {
            store.persist(&HashMap::new())?;
        } else {
            fs::set_permissions(&store.path, fs::Permissions::from_mode(0o600))
                .map_err(io_error)?;
        }
        Ok(store)
    }

    fn persist(&self, values: &HashMap<String, String>) -> Result<(), CredentialError> {
        let mut file = NamedTempFile::new_in(&self.dir).map_err(io_error)?;
        serde_json::to_writer(&mut file, values).map_err(|error| CredentialError::Io {
            detail: error.to_string(),
        })?;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
        file.persist(&self.path)
            .map_err(|error| io_error(error.error))?;
        Ok(())
    }
}

impl CredentialStore for FileCredentialStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        let values = self.values.read().map_err(lock_error)?;
        Ok(values.get(key).cloned().map(Secret::from))
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        let mut values = self.values.write().map_err(lock_error)?;
        values.insert(key.to_owned(), value.expose().to_owned());
        self.persist(&values)
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        let mut values = self.values.write().map_err(lock_error)?;
        values.remove(key);
        self.persist(&values)
    }
}

#[cfg(not(feature = "keychain"))]
pub fn open_default(dir: impl AsRef<Path>) -> Result<Arc<dyn CredentialStore>, CredentialError> {
    Ok(Arc::new(FileCredentialStore::open(dir)?))
}

#[cfg(feature = "keychain")]
pub fn open_default(dir: impl AsRef<Path>) -> Result<Arc<dyn CredentialStore>, CredentialError> {
    match crate::KeyringCredentialStore::probe() {
        Ok(store) => Ok(Arc::new(store)),
        Err(error) => {
            tracing::info!(%error, "資格情報サービスを利用できないためファイルへ切り替えます");
            Ok(Arc::new(FileCredentialStore::open(dir)?))
        }
    }
}

fn io_error(error: std::io::Error) -> CredentialError {
    CredentialError::Io {
        detail: error.to_string(),
    }
}

fn lock_error<T>(error: std::sync::PoisonError<T>) -> CredentialError {
    CredentialError::Io {
        detail: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::MetadataExt};

    use tempfile::tempdir;

    use super::*;

    // Given: 空のファイルストア / When: 設定・取得・削除 / Then: 値のライフサイクルが保存される
    #[test]
    fn round_trip_set_get_delete() {
        let dir = tempdir().expect("一時ディレクトリを作成できるはずです");
        let store = FileCredentialStore::open(dir.path()).expect("ストアを開けるはずです");
        store
            .set("token", &Secret::from("secret".to_owned()))
            .expect("設定できるはずです");
        assert_eq!(
            store
                .get("token")
                .expect("取得できるはずです")
                .expect("値があるはずです")
                .expose(),
            "secret"
        );
        store.delete("token").expect("削除できるはずです");
        assert!(store.get("token").expect("取得できるはずです").is_none());
    }

    // Given: 未登録キー / When: 取得 / Then: 値なしを返す
    #[test]
    fn unknown_key_returns_none() {
        let dir = tempdir().expect("一時ディレクトリを作成できるはずです");
        let store = FileCredentialStore::open(dir.path()).expect("ストアを開けるはずです");
        assert!(store.get("missing").expect("取得できるはずです").is_none());
    }

    // Given: 新規ストア / When: ファイル情報を確認 / Then: ディレクトリとファイルの権限が制限される
    #[test]
    fn permissions_are_restricted() {
        let parent = tempdir().expect("一時ディレクトリを作成できるはずです");
        let dir = parent.path().join("credentials");
        FileCredentialStore::open(&dir).expect("ストアを開けるはずです");
        assert_eq!(
            fs::metadata(&dir).expect("情報を取得できるはずです").mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(dir.join("credentials.json"))
                .expect("情報を取得できるはずです")
                .mode()
                & 0o777,
            0o600
        );
    }

    // Given: 不正な JSON ファイル / When: ストアを開く / Then: 破損エラーになる
    #[test]
    fn malformed_file_is_rejected() {
        let dir = tempdir().expect("一時ディレクトリを作成できるはずです");
        fs::write(dir.path().join("credentials.json"), b"{")
            .expect("不正な fixture を書けるはずです");
        assert!(matches!(
            FileCredentialStore::open(dir.path()),
            Err(CredentialError::Malformed { .. })
        ));
    }

    // Given: 秘密値 / When: Debug 表示 / Then: 内容が伏せられる
    #[test]
    fn secret_debug_is_redacted() {
        assert_eq!(
            format!("{:?}", Secret::from("visible-never".to_owned())),
            "Secret(<redacted>)"
        );
    }

    // Given: 保存済みストア / When: 再度開く / Then: 値が永続化されている
    #[test]
    fn reopen_preserves_values() {
        let dir = tempdir().expect("一時ディレクトリを作成できるはずです");
        FileCredentialStore::open(dir.path())
            .expect("ストアを開けるはずです")
            .set("token", &Secret::from("persisted".to_owned()))
            .expect("設定できるはずです");
        let reopened = FileCredentialStore::open(dir.path()).expect("再度開けるはずです");
        assert_eq!(
            reopened
                .get("token")
                .expect("取得できるはずです")
                .expect("値があるはずです")
                .expose(),
            "persisted"
        );
    }
}
