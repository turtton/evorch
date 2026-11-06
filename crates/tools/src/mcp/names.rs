//! Deterministic, provider-safe external names. A public name is never a codec
//! for its remote key: delimiters in server/tool names are deliberately legal.
use super::types::{MAX_PUBLIC_NAME_BYTES, RemoteToolKey};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpNameError {
    #[error("empty MCP remote key")]
    EmptyKey,
    #[error("duplicate MCP remote key")]
    DuplicateRemoteKey,
    #[error("MCP public name collision")]
    Collision,
}

/// Built from the entire accepted catalog, before registering any tools. Both
/// sides of a readable-name collision are hashed, independent of listing order.
#[derive(Debug, Clone)]
pub struct McpNameBindings {
    by_public: BTreeMap<String, RemoteToolKey>,
    by_remote: HashMap<RemoteToolKey, String>,
}

impl McpNameBindings {
    pub fn new(keys: impl IntoIterator<Item = RemoteToolKey>) -> Result<Self, McpNameError> {
        Self::build(keys, key_digest)
    }

    fn build(
        keys: impl IntoIterator<Item = RemoteToolKey>,
        digest: impl Fn(&RemoteToolKey) -> [u8; 32],
    ) -> Result<Self, McpNameError> {
        let mut candidates: BTreeMap<String, Vec<RemoteToolKey>> = BTreeMap::new();
        let mut unique = std::collections::HashSet::new();
        for key in keys {
            if key.server_id.is_empty() || key.tool_name.is_empty() {
                return Err(McpNameError::EmptyKey);
            }
            if !unique.insert(key.clone()) {
                return Err(McpNameError::DuplicateRemoteKey);
            }
            let candidate = readable_name(&key).unwrap_or_else(|| hashed_name(digest(&key)));
            candidates.entry(candidate).or_default().push(key);
        }
        let mut by_public = BTreeMap::new();
        let mut by_remote = HashMap::new();
        for (candidate, keys) in candidates {
            let collision = keys.len() > 1;
            for key in keys {
                let name = if collision {
                    hashed_name(digest(&key))
                } else {
                    candidate.clone()
                };
                if by_public.insert(name.clone(), key.clone()).is_some() {
                    return Err(McpNameError::Collision);
                }
                by_remote.insert(key, name);
            }
        }
        Ok(Self {
            by_public,
            by_remote,
        })
    }

    pub fn public_name(&self, key: &RemoteToolKey) -> Option<&str> {
        self.by_remote.get(key).map(String::as_str)
    }

    pub fn remote_key(&self, public_name: &str) -> Option<&RemoteToolKey> {
        self.by_public.get(public_name)
    }

    /// Provider catalog ordering, not discovery/registration ordering.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &RemoteToolKey)> {
        self.by_public
            .iter()
            .map(|(name, key)| (name.as_str(), key))
    }
}

fn readable_name(key: &RemoteToolKey) -> Option<String> {
    let allowed = |value: &str| {
        !value.is_empty()
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    };
    if !allowed(&key.server_id) || !allowed(&key.tool_name) {
        return None;
    }
    let name = format!("mcp__{}__{}", key.server_id, key.tool_name);
    (name.len() <= MAX_PUBLIC_NAME_BYTES).then_some(name)
}

fn key_digest(key: &RemoteToolKey) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"evorch.mcp.external-name.v1\0");
    for part in [&key.server_id, &key.tool_name] {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().into()
}

fn hashed_name(digest: [u8; 32]) -> String {
    format!("mcp_h__{}", URL_SAFE_NO_PAD.encode(digest))
}

/// Restore accepts a historical hash even when its original collision partner
/// is absent. It must not rename an already-sent snapshot or reverse-parse it.
pub(super) fn name_matches_key(name: &str, key: &RemoteToolKey) -> bool {
    !key.server_id.is_empty()
        && !key.tool_name.is_empty()
        && (readable_name(key).as_deref() == Some(name) || hashed_name(key_digest(key)) == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(server: &str, tool: &str) -> RemoteToolKey {
        RemoteToolKey {
            server_id: server.into(),
            tool_name: tool.into(),
        }
    }

    #[test]
    fn external_names_use_hash_without_sanitizing() {
        let keys = [
            key("files", "read"),
            key("files", "read.file"),
            key("files", "read_file"),
            key("日本語", "read"),
            key("files", "with space"),
        ];
        let names = McpNameBindings::new(keys.clone()).unwrap();
        assert_eq!(names.public_name(&keys[0]), Some("mcp__files__read"));
        assert_eq!(names.public_name(&keys[2]), Some("mcp__files__read_file"));
        for remote in [&keys[1], &keys[3], &keys[4]] {
            let name = names.public_name(remote).unwrap();
            assert!(name.starts_with("mcp_h__"));
            assert_eq!(name.len(), 50);
            assert_eq!(names.remote_key(name), Some(remote));
        }
    }

    #[test]
    fn external_names_fit_provider_constraints() {
        // Canonical API-key names: ASCII [-_A-Za-z0-9], <=64 bytes. Claude's
        // existing presentation adds a fixed prefix and permits <=128 bytes.
        let at_boundary = key("s", &"t".repeat(56));
        let over_boundary = key("s", &"t".repeat(57));
        let names = McpNameBindings::new([
            at_boundary.clone(),
            over_boundary.clone(),
            key("🦀", &"x".repeat(512)),
        ])
        .unwrap();
        assert_eq!(names.public_name(&at_boundary).unwrap().len(), 64);
        assert!(
            names
                .public_name(&over_boundary)
                .unwrap()
                .starts_with("mcp_h__")
        );
        for (name, _) in names.iter() {
            assert!(name.len() <= MAX_PUBLIC_NAME_BYTES);
            assert!(
                name.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            );
            assert!(format!("_mcp__evorch__{name}").len() <= 128);
            assert!(!name.starts_with("_mcp__evorch"));
        }
    }

    #[test]
    fn ambiguous_readable_names_hash_both_keys_independently_of_order() {
        let keys = [key("a__b", "c"), key("a", "b__c"), key("a", "c")];
        let first = McpNameBindings::new(keys.clone()).unwrap();
        let second = McpNameBindings::new(keys.into_iter().rev()).unwrap();
        assert_eq!(first.by_public, second.by_public);
        for (name, remote) in first.iter() {
            if remote.tool_name != "c" || remote.server_id != "a" {
                assert!(name.starts_with("mcp_h__"));
            } else {
                assert_eq!(name, "mcp__a__c");
            }
        }
        assert!(first.remote_key("mcp__a__b__c").is_none());
    }

    #[test]
    fn final_hash_collisions_and_duplicate_remote_keys_fail_closed() {
        assert!(matches!(
            McpNameBindings::build([key("a.", "b"), key("a", ".b")], |_| [0; 32]),
            Err(McpNameError::Collision)
        ));
        assert!(matches!(
            McpNameBindings::new([key("a", "b"), key("a", "b")]),
            Err(McpNameError::DuplicateRemoteKey)
        ));
        assert!(matches!(
            McpNameBindings::new([key("", "b")]),
            Err(McpNameError::EmptyKey)
        ));
    }

    #[test]
    fn tuple_hash_uses_lengths_and_preserves_namespace_separation() {
        let a = key("a.", "bc");
        let b = key("a", ".bc");
        assert_ne!(key_digest(&a), key_digest(&b));
        let names = McpNameBindings::new([a.clone(), b, key("h", "a")]).unwrap();
        assert!(names.public_name(&a).unwrap().starts_with("mcp_h__"));
        assert_eq!(names.public_name(&key("h", "a")), Some("mcp__h__a"));
        assert!(name_matches_key(names.public_name(&a).unwrap(), &a));
        assert!(!name_matches_key("mcp__a__bc", &a));
    }
}
