use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::ProviderError;

/// Filename within the application's user configuration directory.
pub const CLAUDE_INSTALLATION_ID_FILE: &str = "claude-installation-id";

/// Persistent, credential-independent seed for Claude's installation identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaudeInstallationId(uuid::Uuid);

impl ClaudeInstallationId {
    /// Load a UUIDv4 seed or atomically publish a new one without replacing a winner.
    /// Invalid existing state is an error: rotating it would change device identity.
    pub fn load_or_create(path: &Path) -> Result<Self, ProviderError> {
        if let Some(identity) = read_identity(path)? {
            return Ok(identity);
        }
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        fs::create_dir_all(directory).map_err(|_| identity_error())?;
        let identity = Self(uuid::Uuid::new_v4());
        let mut temporary =
            tempfile::NamedTempFile::new_in(directory).map_err(|_| identity_error())?;
        writeln!(temporary, "{}", identity.0).map_err(|_| identity_error())?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|_| identity_error())?;
        match temporary.persist_noclobber(path) {
            Ok(_) => Ok(identity),
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                read_identity(path)?.ok_or_else(identity_error)
            }
            Err(_) => Err(identity_error()),
        }
    }
}

fn read_identity(path: &Path) -> Result<Option<ClaudeInstallationId>, ProviderError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(identity_error()),
    };
    let mut text = String::new();
    file.take(129)
        .read_to_string(&mut text)
        .map_err(|_| identity_error())?;
    if text.len() > 128 {
        return Err(identity_error());
    }
    let value = uuid::Uuid::parse_str(text.trim()).map_err(|_| identity_error())?;
    if value.get_version_num() != 4 || value.get_variant() != uuid::Variant::RFC4122 {
        return Err(identity_error());
    }
    Ok(Some(ClaudeInstallationId(value)))
}

fn identity_error() -> ProviderError {
    ProviderError::Request("Could not load or create Claude installation identity".into())
}

/// Library callers without an application config path share one process-local seed.
pub(in crate::provider) fn process_installation_id() -> ClaudeInstallationId {
    static ID: OnceLock<ClaudeInstallationId> = OnceLock::new();
    *ID.get_or_init(|| ClaudeInstallationId(uuid::Uuid::new_v4()))
}

#[derive(Serialize)]
struct RequestIdentity<'a> {
    device_id: String,
    session_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    account_uuid: Option<&'a str>,
}

/// OMP's device derivation and JSON field order; sessions retain evorch run affinity.
pub(in crate::provider) fn request_metadata(
    installation_id: &ClaudeInstallationId,
    account_id: Option<&str>,
    fallback_session: &str,
    run_id: Option<&str>,
) -> serde_json::Value {
    let account_id = account_id.filter(|id| !id.trim().is_empty());
    let mut device_hash = Sha256::new();
    if let Some(account_id) = account_id {
        device_hash.update(b"omp-claude-device-id-v2\0");
        device_hash.update(installation_id.0.to_string().as_bytes());
        device_hash.update([0]);
        device_hash.update(account_id.as_bytes());
    } else {
        device_hash.update(b"omp-claude-device-id-v1:");
        device_hash.update(installation_id.0.to_string().as_bytes());
    }
    let device_id = device_hash
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let session_id = run_id.filter(|id| !id.is_empty()).map_or_else(
        || fallback_session.into(),
        |id| {
            let hash = Sha256::digest(format!("evorch-claude-session-v1\0{id}"));
            let mut bytes = [0u8; 16];
            bytes.copy_from_slice(&hash[..16]);
            bytes[6] = (bytes[6] & 0x0f) | 0x40;
            bytes[8] = (bytes[8] & 0x3f) | 0x80;
            uuid::Uuid::from_bytes(bytes).to_string()
        },
    );
    let identity = RequestIdentity {
        device_id,
        session_id,
        account_uuid: account_id,
    };
    // This struct contains only strings, so serialization cannot fail.
    serde_json::json!({"user_id": serde_json::to_string(&identity).expect("string identity serializes")})
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(value: &str) -> ClaudeInstallationId {
        ClaudeInstallationId(uuid::Uuid::parse_str(value).unwrap())
    }

    fn metadata(
        id: &ClaudeInstallationId,
        account: Option<&str>,
        run: Option<&str>,
    ) -> serde_json::Value {
        let metadata = request_metadata(id, account, "33333333-3333-4333-8333-333333333333", run);
        serde_json::from_str(metadata["user_id"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn omp_device_vectors_and_json_order_are_preserved() {
        let id = identity("11111111-1111-4111-8111-111111111111");
        assert_eq!(
            metadata(&id, None, None)["device_id"],
            "859de956c8ecf6a7782133f5309b0dac43195d0a3879afe331eef73d68c8eadd"
        );
        assert_eq!(
            metadata(&id, Some("account-a"), None)["device_id"],
            "f929edd589bd7b6a37fe03e4147f0707b8122e92f36c11b8e58093ee214bce94"
        );
        let other = identity("22222222-2222-4222-8222-222222222222");
        assert_eq!(
            metadata(&other, Some("account-a"), None)["device_id"],
            "21e663c8703d1f4138e3ecf95ce9b3f0177d684a305f51c910aa8f032a164181"
        );
        let encoded = request_metadata(&id, Some("account-a"), "session", None);
        assert_eq!(
            encoded["user_id"],
            "{\"device_id\":\"f929edd589bd7b6a37fe03e4147f0707b8122e92f36c11b8e58093ee214bce94\",\"session_id\":\"session\",\"account_uuid\":\"account-a\"}"
        );
        assert_eq!(metadata(&id, Some("  "), None), metadata(&id, None, None));
        assert_ne!(
            metadata(&id, Some("account-b"), None)["device_id"],
            metadata(&id, Some("account-a"), None)["device_id"]
        );
    }

    #[test]
    fn seed_reopens_and_concurrent_creators_share_the_complete_winner() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/claude-installation-id");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let identities = std::thread::scope(|scope| {
            let handles = (0..8)
                .map(|_| {
                    let barrier = barrier.clone();
                    let path = &path;
                    scope.spawn(move || {
                        barrier.wait();
                        ClaudeInstallationId::load_or_create(path).unwrap()
                    })
                })
                .collect::<Vec<_>>();
            handles
                .into_iter()
                .map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert!(identities.iter().all(|id| *id == identities[0]));
        assert_eq!(
            ClaudeInstallationId::load_or_create(&path).unwrap(),
            identities[0]
        );
        assert_ne!(
            ClaudeInstallationId::load_or_create(&directory.path().join("other/seed")).unwrap(),
            identities[0]
        );
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
    }

    #[test]
    fn corrupt_existing_seed_is_sanitized_and_never_rotated() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("seed");
        for invalid in [
            "private-sentinel",
            "00000000-0000-0000-0000-000000000000",
            "11111111-1111-5111-8111-111111111111",
        ] {
            fs::write(&path, invalid).unwrap();
            let error = ClaudeInstallationId::load_or_create(&path).unwrap_err();
            assert!(!format!("{error:?} {error}").contains(invalid));
            assert_eq!(fs::read_to_string(&path).unwrap(), invalid);
        }
    }

    #[test]
    fn process_seed_and_run_sessions_survive_client_recreation() {
        let first = process_installation_id();
        let second = process_installation_id();
        assert_eq!(first, second);
        let a = request_metadata(&first, Some("account-a"), "first-client", Some("run-a"));
        let b = request_metadata(&second, Some("account-a"), "second-client", Some("run-a"));
        assert_eq!(a, b);
        let a = metadata(&first, Some("account-a"), Some("run-a"));
        let b = metadata(&first, Some("account-a"), Some("run-b"));
        assert_eq!(a["device_id"], b["device_id"]);
        assert_ne!(a["session_id"], b["session_id"]);
        assert!(uuid::Uuid::parse_str(a["session_id"].as_str().unwrap()).is_ok());
    }
}
