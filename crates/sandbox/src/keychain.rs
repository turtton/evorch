//! OS の資格情報サービスを利用するストアです。

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, mpsc},
    thread,
};

use keyring::{Entry, Error};

use crate::{CredentialError, CredentialStore, Secret};

const SERVICE: &str = "evorch";
const PROBE_KEY: &str = "__evorch_probe__";

/// OS の資格情報サービスへ保存するストア。
#[derive(Debug, Clone)]
pub struct KeyringCredentialStore {
    requests: Arc<mpsc::SyncSender<Request>>,
}

/// worker 内だけで資格情報を操作する差し替え可能な境界。
trait KeyringBackend: Send + 'static {
    fn get(&mut self, key: String) -> Result<String, Error>;
    fn set(&mut self, key: String, value: String) -> Result<(), Error>;
    fn delete(&mut self, key: String) -> Result<(), Error>;
}

/// Entry を呼び出し側へ持ち出さない OS backend。
struct OsKeyring;

impl KeyringBackend for OsKeyring {
    fn get(&mut self, key: String) -> Result<String, Error> {
        Entry::new(SERVICE, &key)?.get_password()
    }
    fn set(&mut self, key: String, value: String) -> Result<(), Error> {
        Entry::new(SERVICE, &key)?.set_password(&value)
    }
    fn delete(&mut self, key: String) -> Result<(), Error> {
        Entry::new(SERVICE, &key)?.delete_credential()
    }
}

/// 所有された文字列だけを運ぶ操作。
enum Operation {
    Get(String),
    Set(String, String),
    Delete(String),
}

/// 操作と、その呼び出し専用の応答先。
struct Request {
    operation: Operation,
    reply: mpsc::Sender<Result<Option<String>, CredentialError>>,
}

impl KeyringCredentialStore {
    /// 専用スレッドを起動し、読み取りでサービスの可用性を確認する。
    pub fn probe() -> Result<Self, CredentialError> {
        let store = Self::spawn(OsKeyring)?;
        store.get(PROBE_KEY)?;
        Ok(store)
    }

    /// backend を一つの通常スレッドへ移し、全操作を直列化する。
    fn spawn(mut backend: impl KeyringBackend) -> Result<Self, CredentialError> {
        let (requests, receiver) = mpsc::sync_channel::<Request>(0);
        thread::Builder::new()
            .name("evorch-keyring".to_owned())
            .spawn(move || {
                for request in receiver {
                    let result = catch_unwind(AssertUnwindSafe(|| match request.operation {
                        Operation::Get(key) => match backend.get(key) {
                            Ok(value) => Ok(Some(value)),
                            Err(Error::NoEntry) => Ok(None),
                            Err(error) => Err(keychain_error(error)),
                        },
                        Operation::Set(key, value) => backend
                            .set(key, value)
                            .map(|()| None)
                            .map_err(keychain_error),
                        Operation::Delete(key) => match backend.delete(key) {
                            Ok(()) | Err(Error::NoEntry) => Ok(None),
                            Err(error) => Err(keychain_error(error)),
                        },
                    }))
                    .unwrap_or_else(|_| Err(unavailable("資格情報の操作が panic しました")));
                    // 応答先の終了は他の呼び出しの処理を妨げない。
                    if request.reply.send(result).is_err() {
                        continue;
                    }
                }
            })
            .map_err(|error| unavailable(&error.to_string()))?;
        Ok(Self {
            requests: Arc::new(requests),
        })
    }

    /// 要求を送信し、チャネル切断を利用不可エラーへ変換する。
    fn dispatch(&self, operation: Operation) -> Result<Option<String>, CredentialError> {
        let (reply, response) = mpsc::channel();
        self.requests
            .send(Request { operation, reply })
            .map_err(|_| unavailable("資格情報 worker が終了しています"))?;
        response
            .recv()
            .map_err(|_| unavailable("資格情報 worker の応答がありません"))?
    }
}

impl CredentialStore for KeyringCredentialStore {
    fn get(&self, key: &str) -> Result<Option<Secret>, CredentialError> {
        self.dispatch(Operation::Get(key.to_owned()))
            .map(|value| value.map(Secret::from))
    }

    fn set(&self, key: &str, value: &Secret) -> Result<(), CredentialError> {
        self.dispatch(Operation::Set(key.to_owned(), value.expose().to_owned()))
            .map(|_| ())
    }

    fn delete(&self, key: &str) -> Result<(), CredentialError> {
        self.dispatch(Operation::Delete(key.to_owned())).map(|_| ())
    }
}

fn keychain_error(error: Error) -> CredentialError {
    unavailable(&error.to_string())
}

/// 秘密値や panic ペイロードを含めず利用不可を伝える。
fn unavailable(detail: &str) -> CredentialError {
    CredentialError::KeychainUnavailable {
        detail: detail.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::HashMap,
        sync::{Arc, mpsc},
        thread,
    };
    use tempfile::tempdir;

    use super::*;

    /// 実行スレッドを検査し、要求した操作だけを panic させるメモリストア。
    #[derive(Default)]
    struct FakeBackend {
        values: HashMap<String, String>,
        thread: Option<thread::ThreadId>,
    }

    impl FakeBackend {
        fn check(&mut self, key: &str) {
            assert!(tokio::runtime::Handle::try_current().is_err());
            let current = thread::current().id();
            assert_eq!(*self.thread.get_or_insert(current), current);
            assert_ne!(key, "panic", "意図した backend panic");
        }
    }

    impl KeyringBackend for FakeBackend {
        fn get(&mut self, key: String) -> Result<String, Error> {
            self.check(&key);
            self.values.get(&key).cloned().ok_or(Error::NoEntry)
        }
        fn set(&mut self, key: String, value: String) -> Result<(), Error> {
            self.check(&key);
            self.values.insert(key, value);
            Ok(())
        }
        fn delete(&mut self, key: String) -> Result<(), Error> {
            self.check(&key);
            self.values.remove(&key).map(|_| ()).ok_or(Error::NoEntry)
        }
    }

    // Given: panic する backend / When: 各操作が panic / Then: エラーへ変換され次の要求も成功する
    #[test]
    fn worker_survives_panicking_operations() -> Result<(), CredentialError> {
        let store = KeyringCredentialStore::spawn(FakeBackend::default())?;
        let secret = Secret::from("retained".to_owned());
        store.set("token", &secret)?;
        for result in [
            store.get("panic").map(|_| ()),
            store.set("panic", &secret),
            store.delete("panic"),
        ] {
            assert!(matches!(
                result,
                Err(CredentialError::KeychainUnavailable { .. })
            ));
            assert_eq!(store.get("token")?, Some(secret.clone()));
        }
        Ok(())
    }

    // Given: 同じ worker を共有するハンドル / When: runtime 内から読み書き / Then: 一つの専用スレッドで往復する
    #[test]
    fn worker_round_trip_from_runtime() -> Result<(), Box<dyn std::error::Error>> {
        let store = KeyringCredentialStore::spawn(FakeBackend::default())?;
        let clone = store.clone();
        drop(store);
        tokio::runtime::Builder::new_current_thread()
            .build()?
            .block_on(async {
                let secret = Secret::from("secret".to_owned());
                clone.set("token", &secret)?;
                assert_eq!(clone.get("token")?, Some(secret));
                clone.delete("token")?;
                assert_eq!(clone.get("token")?, None);
                Ok::<_, CredentialError>(())
            })?;
        Ok(())
    }

    // Given: NoEntry を返す backend / When: 未登録キーを取得・削除 / Then: 欠損は成功として扱われる
    #[test]
    fn no_entry_mapping_is_preserved() -> Result<(), CredentialError> {
        let store = KeyringCredentialStore::spawn(FakeBackend::default())?;
        assert_eq!(store.get("missing")?, None);
        store.delete("missing")?;
        Ok(())
    }

    // Given: 終了済み worker / When: 各操作を要求 / Then: panic せず利用不可を返す
    #[test]
    fn disconnected_worker_returns_errors() {
        let (requests, receiver) = mpsc::sync_channel(0);
        drop(receiver);
        let store = KeyringCredentialStore {
            requests: Arc::new(requests),
        };
        for result in [
            store.get("token").map(|_| ()),
            store.set("token", &Secret::from("secret".to_owned())),
            store.delete("token"),
        ] {
            assert!(matches!(
                result,
                Err(CredentialError::KeychainUnavailable { .. })
            ));
        }
    }

    // Given: 利用可能な実 keyring / When: 両 runtime の内部から操作 / Then: ネストした runtime panic を起こさない
    #[test]
    fn real_keyring_works_inside_both_runtimes() -> Result<(), Box<dyn std::error::Error>> {
        let store = match KeyringCredentialStore::probe() {
            Ok(store) => store,
            Err(error) => {
                eprintln!("実 keyring テストをスキップ: {error}");
                return Ok(());
            }
        };
        let unique = tempdir()?;
        let key = format!("__evorch_runtime_test__{}", unique.path().display());
        for runtime in [
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()?,
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?,
        ] {
            runtime.block_on(async {
                let secret = Secret::from("runtime-test-secret".to_owned());
                store.set(&key, &secret)?;
                let value = store.get(&key);
                store.delete(&key)?;
                assert_eq!(value?, Some(secret));
                assert_eq!(store.get(&key)?, None);
                Ok::<_, CredentialError>(())
            })?;
        }
        eprintln!("実 keyring: 両 runtime の get/set/delete を実行済み");
        Ok(())
    }

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
