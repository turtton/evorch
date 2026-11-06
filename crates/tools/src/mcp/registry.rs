use super::{
    McpError, McpErrorKind, McpTool, McpToolDefinition, McpToolResult,
    names::{McpNameBindings, name_matches_key},
    types::{
        ApprovalFingerprint, DefinitionFingerprint, FrozenMcpSnapshot, FrozenMcpTool, RemoteToolKey,
    },
};
use crate::Permissions;
use async_trait::async_trait;
use std::{
    collections::{BTreeMap, HashSet},
    sync::Arc,
};
use tokio::sync::OnceCell;

/// The host-process capability must not be represented as network-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum McpToolTransport {
    Http,
    Stdio,
}
impl McpToolTransport {
    pub const fn permissions(self) -> Permissions {
        match self {
            Self::Http => Permissions::network(),
            Self::Stdio => Permissions {
                fs_read: true,
                fs_write: true,
                process_spawn: true,
                network: true,
            },
        }
    }
}

/// Discovery already performed for ONE run/server; never a cross-run cache.
/// The owning run lease/connector must retain transport cleanup responsibility.
#[async_trait]
pub trait McpRegistrySession: Send + Sync {
    fn definitions(&self) -> &[McpToolDefinition];
    async fn call_tool(
        &self,
        remote_name: &str,
        args: serde_json::Value,
    ) -> Result<(McpToolResult, serde_json::Value), McpError>;
}

/// Injected at runtime integration; construction and metadata access are inert.
/// check_execution rechecks enabled/role/consent/destination before lazy connect
/// AND immediately before every call (including an already-connected session).
#[async_trait]
pub trait McpRegistryConnector: Send + Sync {
    /// Frozen run metadata, not a query of mutable on-disk configuration.
    fn tool_transport(&self, server_id: &str) -> McpToolTransport;
    async fn check_execution(&self, tool: &FrozenMcpTool) -> Result<(), McpError>;
    /// May return this run's already-prepared session. Enforce permit, shared
    /// limiter, cancellation and restart budget here, before any I/O.
    async fn connect(&self, tool: &FrozenMcpTool) -> Result<Arc<dyn McpRegistrySession>, McpError>;
    /// Await cleanup of every session, including those opened lazily on restore.
    async fn shutdown(&self, _reason: super::transport::ShutdownReason) {}
}

pub struct McpServerTools {
    pub server_id: String,
    pub transport: McpToolTransport,
    pub approval_fingerprint: ApprovalFingerprint,
    pub definitions: Vec<McpToolDefinition>,
}

impl FrozenMcpSnapshot {
    /// Freeze accepted definitions once, independent of discovery order. Schema
    /// and annotation validation/exclusion precedes this step in the client.
    pub fn from_servers(
        servers: impl IntoIterator<Item = McpServerTools>,
    ) -> Result<Self, McpError> {
        let mut definitions = BTreeMap::new();
        let mut server_ids = HashSet::new();
        for server in servers {
            if server.server_id.is_empty() {
                return Err(error("catalog", "tools/list", McpErrorKind::Configuration));
            }
            if !server_ids.insert(server.server_id.clone()) {
                return Err(error(
                    &server.server_id,
                    "tools/list",
                    McpErrorKind::Protocol,
                ));
            }
            if server_ids.len() > config::mcp::MAX_SERVERS {
                return Err(error("catalog", "tools/list", McpErrorKind::LimitExceeded));
            }
            if server.definitions.len() > super::types::MAX_TOOLS {
                return Err(error(
                    &server.server_id,
                    "tools/list",
                    McpErrorKind::LimitExceeded,
                ));
            }
            for definition in server.definitions {
                let key = RemoteToolKey {
                    server_id: server.server_id.clone(),
                    tool_name: definition.name.clone(),
                };
                if definitions
                    .insert(
                        (key.server_id.clone(), key.tool_name.clone()),
                        (
                            key,
                            definition,
                            server.approval_fingerprint,
                            server.transport,
                        ),
                    )
                    .is_some()
                {
                    return Err(error(
                        &server.server_id,
                        "tools/list",
                        McpErrorKind::Protocol,
                    ));
                }
            }
        }
        let bindings = McpNameBindings::new(definitions.values().map(|(key, _, _, _)| key.clone()))
            .map_err(|_| error("catalog", "tools/list", McpErrorKind::Configuration))?;
        let mut tools = Vec::with_capacity(definitions.len());
        for (public_name, key) in bindings.iter() {
            let (_, definition, approval_fingerprint, transport) = definitions
                .remove(&(key.server_id.clone(), key.tool_name.clone()))
                .expect("binding was built from these definitions");
            tools.push(FrozenMcpTool {
                public_name: public_name.into(),
                transport,
                remote: key.clone(),
                definition_fingerprint: DefinitionFingerprint::for_definition(&definition),
                definition,
                approval_fingerprint,
            });
        }
        Ok(Self { tools })
    }
}

struct FrozenRegistry {
    snapshot: FrozenMcpSnapshot,
    connector: Arc<dyn McpRegistryConnector>,
    sessions: BTreeMap<String, OnceCell<Arc<dyn McpRegistrySession>>>,
    transports: BTreeMap<String, McpToolTransport>,
}

pub(super) enum McpToolBinding {
    Frozen(Arc<FrozenMcpTool>),
}
impl McpToolBinding {
    pub(super) fn definition(&self) -> &McpToolDefinition {
        match self {
            Self::Frozen(tool) => &tool.definition,
        }
    }
    pub(super) fn public_name(&self) -> &str {
        match self {
            Self::Frozen(tool) => &tool.public_name,
        }
    }
}

/// Construction/registration never discovers. External project servers use
/// explicit snapshot bindings and a checked connector via `from_snapshot`.
pub struct McpToolRegistry {
    backend: FrozenRegistry,
}
impl McpToolRegistry {
    /// Validate saved bindings without sorting, rewriting or contacting a server.
    pub fn from_snapshot(
        snapshot: FrozenMcpSnapshot,
        connector: Arc<dyn McpRegistryConnector>,
    ) -> Result<Self, McpError> {
        let mut names = HashSet::new();
        let mut remote_keys = HashSet::new();
        let mut approvals = BTreeMap::new();
        let mut counts = BTreeMap::<String, usize>::new();
        let mut sessions = BTreeMap::new();
        let mut transports = BTreeMap::new();
        for tool in &snapshot.tools {
            let count = counts.entry(tool.remote.server_id.clone()).or_default();
            *count += 1;
            if *count > super::types::MAX_TOOLS || counts.len() > config::mcp::MAX_SERVERS {
                return Err(error("catalog", "tools/list", McpErrorKind::LimitExceeded));
            }
            if !name_matches_key(&tool.public_name, &tool.remote)
                || tool.definition.name != tool.remote.tool_name
                || tool.definition_fingerprint
                    != DefinitionFingerprint::for_definition(&tool.definition)
                || !names.insert(tool.public_name.clone())
                || !remote_keys.insert(tool.remote.clone())
            {
                return Err(error(
                    &tool.remote.server_id,
                    "tools/list",
                    McpErrorKind::Configuration,
                ));
            }
            if approvals
                .insert(tool.remote.server_id.clone(), tool.approval_fingerprint)
                .is_some_and(|previous| previous != tool.approval_fingerprint)
            {
                return Err(error(
                    &tool.remote.server_id,
                    "tools/list",
                    McpErrorKind::Configuration,
                ));
            }
            sessions
                .entry(tool.remote.server_id.clone())
                .or_insert_with(OnceCell::const_new);
            if transports
                .insert(tool.remote.server_id.clone(), tool.transport)
                .is_some_and(|previous| previous != tool.transport)
            {
                return Err(error(
                    &tool.remote.server_id,
                    "tools/list",
                    McpErrorKind::Configuration,
                ));
            }
        }
        Ok(Self {
            backend: FrozenRegistry {
                snapshot,
                connector,
                sessions,
                transports,
            },
        })
    }

    pub async fn shutdown(&self, reason: super::transport::ShutdownReason) {
        self.backend.connector.shutdown(reason).await;
    }

    pub fn snapshot(&self) -> Option<&FrozenMcpSnapshot> {
        Some(&self.backend.snapshot)
    }

    /// Frozen public order is returned unchanged, including during restore.
    pub fn tools(self: &Arc<Self>) -> Result<Vec<McpTool>, McpError> {
        let registry = &self.backend;
        Ok(registry
            .snapshot
            .tools
            .iter()
            .map(|tool| {
                McpTool::new_frozen(
                    tool.clone(),
                    Arc::clone(self),
                    registry.transports[&tool.remote.server_id],
                )
            })
            .collect())
    }

    pub(super) async fn call(
        &self,
        binding: &McpToolBinding,
        args: serde_json::Value,
    ) -> Result<(McpToolResult, serde_json::Value), McpError> {
        let registry = &self.backend;
        let McpToolBinding::Frozen(tool) = binding;
        registry.connector.check_execution(tool).await?;
        let cell = &registry.sessions[&tool.remote.server_id];
        let session = cell
            .get_or_try_init(|| registry.connector.connect(tool))
            .await?;
        let mut seen = HashSet::new();
        let mut matching = false;
        if session.definitions().len() > super::types::MAX_TOOLS {
            return Err(error(
                &tool.remote.server_id,
                "tools/list",
                McpErrorKind::LimitExceeded,
            ));
        }
        for definition in session.definitions() {
            if !seen.insert(&definition.name) {
                return Err(error(
                    &tool.remote.server_id,
                    "tools/list",
                    McpErrorKind::Protocol,
                ));
            }
            if definition.name == tool.remote.tool_name {
                matching = DefinitionFingerprint::for_definition(definition)
                    == tool.definition_fingerprint;
            }
        }
        if !matching {
            return Err(error(
                &tool.remote.server_id,
                "tools/call",
                McpErrorKind::DefinitionChanged,
            ));
        }
        registry.connector.check_execution(tool).await?;
        // Never retry a call: a failed response can hide a completed side effect.
        session.call_tool(&tool.remote.tool_name, args).await
    }
}
fn error(server: &str, method: &'static str, kind: McpErrorKind) -> McpError {
    McpError {
        server: server.into(),
        method,
        request_id: None,
        kind,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tool;
    use serde_json::{Value, json};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    struct FakeSession {
        definitions: Vec<McpToolDefinition>,
        calls: StdMutex<Vec<(String, Value)>>,
        outcome: StdMutex<Option<McpErrorKind>>,
        tool_error: AtomicBool,
        structured: StdMutex<Option<serde_json::Map<String, Value>>>,
    }
    #[async_trait]
    impl McpRegistrySession for FakeSession {
        fn definitions(&self) -> &[McpToolDefinition] {
            &self.definitions
        }
        async fn call_tool(
            &self,
            name: &str,
            args: Value,
        ) -> Result<(McpToolResult, Value), McpError> {
            self.calls.lock().unwrap().push((name.into(), args));
            if let Some(kind) = *self.outcome.lock().unwrap() {
                return Err(error("fixture", "tools/call", kind));
            }
            Ok((
                McpToolResult {
                    content: vec![super::super::McpToolContent::Text {
                        text: "remote result".into(),
                    }],
                    is_error: self.tool_error.load(Ordering::SeqCst),
                    structured_content: self.structured.lock().unwrap().clone(),
                },
                json!({"server":"fixture","method":"tools/call","class":"ok","status":null}),
            ))
        }
    }
    struct FakeConnector {
        session: Arc<FakeSession>,
        checks: AtomicUsize,
        connects: StdMutex<Vec<String>>,
        denied: StdMutex<Option<McpErrorKind>>,
        revoke_on_connect: AtomicBool,
        transport: StdMutex<McpToolTransport>,
    }
    impl FakeConnector {
        fn new(definitions: Vec<McpToolDefinition>) -> Arc<Self> {
            Arc::new(Self {
                session: Arc::new(FakeSession {
                    definitions,
                    calls: StdMutex::new(Vec::new()),
                    outcome: StdMutex::new(None),
                    tool_error: AtomicBool::new(false),
                    structured: StdMutex::new(None),
                }),
                checks: AtomicUsize::new(0),
                connects: StdMutex::new(Vec::new()),
                denied: StdMutex::new(None),
                revoke_on_connect: AtomicBool::new(false),
                transport: StdMutex::new(McpToolTransport::Http),
            })
        }
    }
    #[async_trait]
    impl McpRegistryConnector for FakeConnector {
        fn tool_transport(&self, _: &str) -> McpToolTransport {
            *self.transport.lock().unwrap()
        }
        async fn check_execution(&self, tool: &FrozenMcpTool) -> Result<(), McpError> {
            self.checks.fetch_add(1, Ordering::SeqCst);
            match *self.denied.lock().unwrap() {
                Some(kind) => Err(error(&tool.remote.server_id, "tools/call", kind)),
                None => Ok(()),
            }
        }
        async fn connect(
            &self,
            tool: &FrozenMcpTool,
        ) -> Result<Arc<dyn McpRegistrySession>, McpError> {
            self.connects
                .lock()
                .unwrap()
                .push(tool.remote.server_id.clone());
            if self.revoke_on_connect.load(Ordering::SeqCst) {
                *self.denied.lock().unwrap() = Some(McpErrorKind::ApprovalRevoked);
            }
            Ok(self.session.clone())
        }
    }
    fn definition(name: &str) -> McpToolDefinition {
        serde_json::from_value(json!({
            "name":name,"description":"The actual remote description",
            "inputSchema":{"type":"object","properties":{"value":{"type":"string"}}}
        }))
        .unwrap()
    }
    fn server(id: &str, definitions: Vec<McpToolDefinition>) -> McpServerTools {
        McpServerTools {
            server_id: id.into(),
            transport: McpToolTransport::Http,
            approval_fingerprint: ApprovalFingerprint([1; 32]),
            definitions,
        }
    }
    fn snapshot(definitions: Vec<McpToolDefinition>) -> FrozenMcpSnapshot {
        FrozenMcpSnapshot::from_servers([server("files", definitions)]).unwrap()
    }
    fn registry(
        snapshot: FrozenMcpSnapshot,
        connector: &Arc<FakeConnector>,
    ) -> Arc<McpToolRegistry> {
        Arc::new(McpToolRegistry::from_snapshot(snapshot, connector.clone()).unwrap())
    }

    #[test]
    fn snapshot_order_and_fingerprints_do_not_depend_on_discovery_order() {
        let a = FrozenMcpSnapshot::from_servers([
            server("z", vec![definition("b"), definition("a")]),
            server("a", vec![definition("日本語")]),
        ])
        .unwrap();
        let b = FrozenMcpSnapshot::from_servers([
            server("a", vec![definition("日本語")]),
            server("z", vec![definition("a"), definition("b")]),
        ])
        .unwrap();
        assert_eq!(
            serde_json::to_vec(&a).unwrap(),
            serde_json::to_vec(&b).unwrap()
        );
        assert!(
            a.tools
                .windows(2)
                .all(|pair| pair[0].public_name < pair[1].public_name)
        );
        assert!(
            FrozenMcpSnapshot::from_servers([server("a", vec![definition("b"), definition("b")])])
                .is_err()
        );
        assert!(
            FrozenMcpSnapshot::from_servers([server("a", vec![]), server("a", vec![])]).is_err()
        );
    }

    #[tokio::test]
    async fn restore_registers_snapshot_without_connecting_or_reordering() {
        let mut saved = snapshot(vec![definition("a"), definition("b")]);
        saved.tools.reverse();
        let bytes = serde_json::to_vec(&saved).unwrap();
        let connector = FakeConnector::new(vec![]);
        let registry = registry(saved, &connector);
        let tools = registry.tools().unwrap();
        assert_eq!(
            tools.iter().map(|tool| tool.name()).collect::<Vec<_>>(),
            vec!["mcp__files__b", "mcp__files__a"]
        );
        assert_eq!(registry.tools().unwrap().len(), 2);
        assert_eq!(
            serde_json::to_vec(registry.snapshot().unwrap()).unwrap(),
            bytes
        );
        assert_eq!(connector.checks.load(Ordering::SeqCst), 0);
        assert!(connector.connects.lock().unwrap().is_empty());
        assert!(connector.session.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn binding_is_not_reverse_parsed() {
        let first = definition("c");
        let second = definition("b__c");
        let saved = FrozenMcpSnapshot::from_servers([
            server("a__b", vec![first.clone()]),
            server("a", vec![second.clone()]),
            server("日本語", vec![definition("x.y")]),
        ])
        .unwrap();
        let connector = FakeConnector::new(vec![first, second, definition("x.y")]);
        let registry = registry(saved.clone(), &connector);
        for (tool, frozen) in registry.tools().unwrap().into_iter().zip(&saved.tools) {
            assert!(tool.name().starts_with("mcp_h__"));
            assert_eq!(
                tool.schema(),
                Value::Object(frozen.definition.input_schema.clone())
            );
            assert_eq!(
                tool.description(),
                frozen.definition.description.as_deref().unwrap()
            );
            assert!(
                !tool
                    .execute(json!({"value":"kept"}))
                    .await
                    .unwrap()
                    .is_error
            );
            let calls = connector.session.calls.lock().unwrap();
            assert_eq!(
                calls.last(),
                Some(&(frozen.remote.tool_name.clone(), json!({"value":"kept"})))
            );
        }
        assert_eq!(connector.connects.lock().unwrap().len(), 3);
    }

    #[test]
    fn restore_keeps_historical_hash_when_collision_partner_is_absent() {
        let mut saved = FrozenMcpSnapshot::from_servers([
            server("a__b", vec![definition("c")]),
            server("a", vec![definition("b__c")]),
        ])
        .unwrap();
        saved.tools.truncate(1);
        let public_name = saved.tools[0].public_name.clone();
        assert!(public_name.starts_with("mcp_h__"));
        let connector = FakeConnector::new(vec![]);
        let registry = registry(saved, &connector);
        assert_eq!(registry.tools().unwrap()[0].name(), public_name);
        assert!(connector.connects.lock().unwrap().is_empty());
    }

    #[test]
    fn catalog_limits_are_not_silently_truncated() {
        let accepted = snapshot(
            (0..super::super::types::MAX_TOOLS)
                .map(|index| definition(&format!("tool{index}")))
                .collect(),
        );
        assert_eq!(accepted.tools.len(), super::super::types::MAX_TOOLS);
        let connector = FakeConnector::new(vec![]);
        assert!(McpToolRegistry::from_snapshot(accepted.clone(), connector.clone()).is_ok());
        let mut restored = accepted;
        restored
            .tools
            .extend(snapshot(vec![definition("extra")]).tools);
        assert!(matches!(
            McpToolRegistry::from_snapshot(restored, connector.clone()),
            Err(McpError {
                kind: McpErrorKind::LimitExceeded,
                ..
            })
        ));
        let oversized = FrozenMcpSnapshot::from_servers([server(
            "files",
            vec![definition("read"); super::super::types::MAX_TOOLS + 1],
        )]);
        assert_eq!(oversized.unwrap_err().kind, McpErrorKind::LimitExceeded);
        let accepted = FrozenMcpSnapshot::from_servers(
            (0..config::mcp::MAX_SERVERS)
                .map(|i| server(&format!("s{i}"), vec![definition("read")])),
        )
        .unwrap();
        assert!(McpToolRegistry::from_snapshot(accepted.clone(), connector.clone()).is_ok());
        let mut restored = accepted;
        restored.tools.extend(
            FrozenMcpSnapshot::from_servers([server("extra", vec![definition("read")])])
                .unwrap()
                .tools,
        );
        assert!(matches!(
            McpToolRegistry::from_snapshot(restored, connector.clone()),
            Err(McpError {
                kind: McpErrorKind::LimitExceeded,
                ..
            })
        ));
        let oversized = FrozenMcpSnapshot::from_servers(
            (0..=config::mcp::MAX_SERVERS).map(|i| server(&format!("s{i}"), vec![])),
        );
        assert_eq!(oversized.unwrap_err().kind, McpErrorKind::LimitExceeded);
        assert_eq!(connector.checks.load(Ordering::SeqCst), 0);
        assert!(connector.connects.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn duplicate_live_definitions_reject_the_server_without_calling() {
        let connector = FakeConnector::new(vec![definition("read"), definition("read")]);
        let registry = registry(snapshot(vec![definition("read")]), &connector);
        let result = registry.tools().unwrap()[0]
            .execute(json!({}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.detail.unwrap()["class"], "protocol");
        assert!(connector.session.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn unapproved_or_revoked_tools_do_not_connect_or_call() {
        let connector = FakeConnector::new(vec![definition("read")]);
        *connector.denied.lock().unwrap() = Some(McpErrorKind::NeedsApproval);
        let registry = registry(snapshot(vec![definition("read")]), &connector);
        let tool = registry.tools().unwrap().pop().unwrap();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.detail.unwrap()["class"], "needs_approval");
        assert!(connector.connects.lock().unwrap().is_empty());
        assert!(connector.session.calls.lock().unwrap().is_empty());
        *connector.denied.lock().unwrap() = None;
        assert!(!tool.execute(json!({})).await.unwrap().is_error);
        *connector.denied.lock().unwrap() = Some(McpErrorKind::ApprovalRevoked);
        assert!(tool.execute(json!({})).await.unwrap().is_error);
        assert_eq!(connector.connects.lock().unwrap().len(), 1);
        assert_eq!(connector.session.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn revocation_during_connect_is_checked_before_remote_call() {
        let connector = FakeConnector::new(vec![definition("read")]);
        connector.revoke_on_connect.store(true, Ordering::SeqCst);
        let registry = registry(snapshot(vec![definition("read")]), &connector);
        let result = registry.tools().unwrap()[0]
            .execute(json!({}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.detail.unwrap()["class"], "approval_revoked");
        assert_eq!(connector.connects.lock().unwrap().len(), 1);
        assert!(connector.session.calls.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn restore_first_call_checks_live_definition_without_changing_snapshot() {
        let original = definition("read");
        let mut description = original.clone();
        description.description = Some("Changed description".into());
        let mut schema = original.clone();
        schema
            .input_schema
            .insert("required".into(), json!(["value"]));
        for live in [description, schema, definition("renamed")] {
            let saved = snapshot(vec![original.clone()]);
            let bytes = serde_json::to_vec(&saved).unwrap();
            let connector = FakeConnector::new(vec![live]);
            let registry = registry(saved, &connector);
            let tool = registry.tools().unwrap().pop().unwrap();
            let result = tool.execute(json!({})).await.unwrap();
            assert!(result.is_error);
            assert_eq!(result.detail.unwrap()["class"], "definition_changed");
            assert_eq!(
                serde_json::to_vec(registry.snapshot().unwrap()).unwrap(),
                bytes
            );
            assert_eq!(tool.description(), original.description.as_deref().unwrap());
            assert!(connector.session.calls.lock().unwrap().is_empty());
        }
    }

    #[tokio::test]
    async fn sessions_are_lazy_once_per_server_and_unknown_calls_are_not_replayed() {
        let connector = FakeConnector::new(vec![definition("a"), definition("b")]);
        let registry = registry(snapshot(vec![definition("a"), definition("b")]), &connector);
        let tools = registry.tools().unwrap();
        let (a, b) = tokio::join!(tools[0].execute(json!({})), tools[1].execute(json!({})));
        assert!(!a.unwrap().is_error && !b.unwrap().is_error);
        assert_eq!(connector.connects.lock().unwrap().len(), 1);
        assert_eq!(connector.session.calls.lock().unwrap().len(), 2);
        *connector.session.outcome.lock().unwrap() = Some(McpErrorKind::OutcomeUnknown);
        let result = tools[0].execute(json!({})).await.unwrap();
        assert!(result.is_error);
        assert_eq!(result.detail.unwrap()["class"], "outcome_unknown");
        assert_eq!(connector.session.calls.lock().unwrap().len(), 3);
        assert_eq!(connector.connects.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn server_tool_error_is_not_a_transport_success() {
        let connector = FakeConnector::new(vec![definition("read")]);
        connector.session.tool_error.store(true, Ordering::SeqCst);
        *connector.session.structured.lock().unwrap() =
            Some(json!({"rejected":true}).as_object().unwrap().clone());
        let registry = registry(snapshot(vec![definition("read")]), &connector);
        let result = registry.tools().unwrap()[0]
            .execute(json!({}))
            .await
            .unwrap();
        assert!(result.is_error);
        assert_eq!(result.content.lines().next(), Some("remote result"));
        assert_eq!(
            serde_json::from_str::<Value>(result.content.lines().last().unwrap()).unwrap(),
            json!({"rejected":true})
        );
        assert_eq!(result.detail.unwrap()["class"], "tool_rejected");
        assert_eq!(connector.session.calls.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn large_structured_results_are_bounded_once_at_return() {
        let connector = FakeConnector::new(vec![definition("read")]);
        *connector.session.structured.lock().unwrap() = Some(
            json!({"payload":"あ".repeat(20_000)})
                .as_object()
                .unwrap()
                .clone(),
        );
        let registry = registry(snapshot(vec![definition("read")]), &connector);
        let result = registry.tools().unwrap()[0]
            .execute(json!({}))
            .await
            .unwrap();
        assert!(!result.is_error);
        assert!(result.content.len() < crate::output::PREVIEW_BYTES + 2048);
        assert!(result.detail.as_ref().unwrap()["output_artifact"]["path"].is_string());
        let again = crate::output::limit_result(result.clone());
        assert_eq!(again.content, result.content);
        assert_eq!(
            again.detail, result.detail,
            "executor must preserve the first artifact reference"
        );
        assert_eq!(connector.session.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn forged_or_inconsistent_snapshots_are_rejected_without_connector_access() {
        let good = snapshot(vec![definition("a"), definition("b")]);
        let mut wrong_name = good.clone();
        wrong_name.tools[0].public_name = "apply_patch".into();
        let mut wrong_binding = good.clone();
        wrong_binding.tools[0].remote.tool_name = "different".into();
        let mut wrong_fingerprint = good.clone();
        wrong_fingerprint.tools[0].definition.description = None;
        let mut duplicate = good.clone();
        duplicate.tools.push(duplicate.tools[0].clone());
        let mut inconsistent_approval = good;
        inconsistent_approval.tools[0].approval_fingerprint = ApprovalFingerprint([2; 32]);
        let connector = FakeConnector::new(vec![]);
        for invalid in [
            wrong_name,
            wrong_binding,
            wrong_fingerprint,
            duplicate,
            inconsistent_approval,
        ] {
            assert!(McpToolRegistry::from_snapshot(invalid, connector.clone()).is_err());
        }
        assert_eq!(connector.checks.load(Ordering::SeqCst), 0);
        assert!(connector.connects.lock().unwrap().is_empty());
    }

    #[test]
    fn stdio_permissions_include_host_process_capabilities() {
        let connector = FakeConnector::new(vec![definition("read")]);
        *connector.transport.lock().unwrap() = McpToolTransport::Stdio;
        let mut saved = snapshot(vec![definition("read")]);
        saved.tools[0].transport = McpToolTransport::Stdio;
        let registry = registry(saved, &connector);
        let permissions = registry.tools().unwrap()[0].permissions();
        assert!(
            permissions.fs_read
                && permissions.fs_write
                && permissions.process_spawn
                && permissions.network
        );
        let http = McpToolTransport::Http.permissions();
        assert!(http.network && !http.process_spawn && !http.fs_read && !http.fs_write);
        *connector.transport.lock().unwrap() = McpToolTransport::Http;
        assert_eq!(registry.tools().unwrap()[0].permissions(), permissions);
    }
}
