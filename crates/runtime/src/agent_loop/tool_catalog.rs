//! Pure fixed tool composition shared by a live loop and planned recovery preview.
use super::tool_calls::{append_subagent_context_note, retain_backed_tools, visible_tool_specs};
use providers::ToolSpec;
pub(crate) struct CatalogOptions<'a> {
    pub skills: bool,
    pub child: bool,
    pub web: bool,
    pub artifacts: bool,
    pub run_js: bool,
    pub run_js_settings: &'a config::RunJsConfig,
    pub family: crate::ModelFamily,
}
pub(crate) fn compose(
    specs: Vec<ToolSpec>,
    policy: &crate::ExecutionPolicy,
    options: CatalogOptions<'_>,
) -> Vec<ToolSpec> {
    let mut specs = visible_tool_specs(specs, policy, options.skills, options.child, options.web);
    retain_backed_tools(&mut specs, options.artifacts);
    super::run_js_host::apply_run_js_specs(&mut specs, options.run_js, options.run_js_settings);
    append_subagent_context_note(&mut specs, options.family);
    specs
}
/// Full build-owned registry, not the role-visible subset. Base executors hold
/// standard/host tools; MCP tools are registered only in disposable run forks.
/// Preserve registry order and overlay exact provider-visible projections (run_js
/// hints, delegate context, edit selection). Hidden builtins retain raw schemas.
pub(crate) fn builtin_catalog(base: &tools::ToolExecutor, effective: &[ToolSpec]) -> Vec<ToolSpec> {
    overlay_builtin_catalog(super::tool_calls::standard_tool_specs(base), effective)
}
/// Both live-build and fixture factories overlay through this exact projection.
pub(crate) fn overlay_builtin_catalog(raw: Vec<ToolSpec>, effective: &[ToolSpec]) -> Vec<ToolSpec> {
    raw.into_iter()
        .map(|raw| {
            effective
                .iter()
                .find(|tool| tool.name == raw.name)
                .cloned()
                .unwrap_or(raw)
        })
        .collect()
}
/// Same frozen-name permission projection used by the real restored loop.
pub(crate) fn allow_saved_mcp_tools(
    policy: &mut crate::ExecutionPolicy,
    snapshot: &tools::mcp::types::FrozenMcpSnapshot,
) {
    policy
        .capabilities
        .allowed_tools
        .extend(snapshot.tools.iter().map(|tool| tool.public_name.clone()));
}
/// Metadata-only restoration into a disposable executor and bus. The lease's
/// unavailable connector cannot do discovery, read credentials or spawn servers.
/// All temporary registration state is dropped before returning the plain specs.
pub(crate) fn restored_specs(
    base: &tools::ToolExecutor,
    snapshot: Option<&tools::mcp::types::FrozenMcpSnapshot>,
) -> Result<Vec<ToolSpec>, tools::mcp::McpErrorKind> {
    let Some(snapshot) = snapshot else {
        return Ok(super::tool_calls::standard_tool_specs(base));
    };
    let lease = crate::mcp::RunMcpLease::restore_snapshot(snapshot.clone());
    let executor =
        lease.register_executor(base, std::sync::Arc::new(event_bus::EventBus::new(4)))?;
    Ok(super::tool_calls::standard_tool_specs(&executor))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot(
        base: &tools::ToolExecutor,
        effective: &[ToolSpec],
    ) -> storage::recovery::restart::tools::ToolSnapshot {
        storage::recovery::restart::tools::ToolSnapshot {
            effective: effective
                .iter()
                .map(|s| serde_json::to_value(s).unwrap())
                .collect(),
            builtins: builtin_catalog(base, effective)
                .iter()
                .map(|s| serde_json::to_value(s).unwrap())
                .collect(),
            stable_context: "unchanged model/history/authority".into(),
        }
    }
    fn boundary(
        old: storage::recovery::restart::tools::ToolSnapshot,
        new: storage::recovery::restart::tools::ToolSnapshot,
    ) {
        use storage::recovery::restart::tools::RestartToolsEvidence;
        let evidence = RestartToolsEvidence::compare(
            "run".into(),
            "incarnation".into(),
            "old-build".into(),
            "new-build".into(),
            old,
            new.clone(),
        )
        .unwrap();
        assert!(evidence.boundary);
        assert!(
            !RestartToolsEvidence::compare(
                "run".into(),
                "incarnation".into(),
                "new-build".into(),
                "new-build".into(),
                new.clone(),
                new
            )
            .unwrap()
            .boundary
        );
    }
    #[test]
    fn full_builtin_catalog_run_js_hint_change_is_an_explained_standard_tool_boundary() {
        use tools::Tool;
        struct OldRead;
        #[async_trait::async_trait]
        impl Tool for OldRead {
            fn name(&self) -> &str {
                "read"
            }
            fn description(&self) -> &str {
                "old read schema"
            }
            fn schema(&self) -> serde_json::Value {
                let mut value = tools::Read.schema();
                value["properties"]["old_build_option"] = serde_json::json!({"type":"boolean"});
                value
            }
            fn permissions(&self) -> tools::Permissions {
                tools::Permissions::read_only()
            }
            async fn execute(
                &self,
                _: serde_json::Value,
            ) -> Result<tools::ToolResult, tools::ToolError> {
                panic!("metadata-only test executed a tool")
            }
        }
        let make = || {
            tools::ToolExecutor::with_standard_tools(
                std::sync::Arc::new(event_bus::EventBus::new(4)),
                std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
            )
        };
        let mut old = make();
        old.register(std::sync::Arc::new(OldRead)).unwrap();
        let new = make();
        let policy = crate::ExecutionPolicy::for_role(crate::Role::Worker);
        let settings = config::RunJsConfig::default();
        let effective = |base: &tools::ToolExecutor| {
            compose(
                super::super::tool_calls::standard_tool_specs(base),
                &policy,
                CatalogOptions {
                    skills: false,
                    child: true,
                    web: false,
                    artifacts: false,
                    run_js: true,
                    run_js_settings: &settings,
                    family: crate::ModelFamily::Gpt5,
                },
            )
        };
        let before = effective(&old);
        let after = effective(&new);
        assert!(before.iter().any(|s| s.name == "run_js"));
        assert_ne!(
            before, after,
            "run_js must reflect the changed compiled read schema"
        );
        for (base, visible) in [(&old, &before), (&new, &after)] {
            let full = builtin_catalog(base, visible);
            assert_eq!(
                full.len(),
                super::super::tool_calls::standard_tool_specs(base).len()
            );
            for spec in visible {
                assert_eq!(full.iter().find(|b| b.name == spec.name), Some(spec));
            }
        }
        boundary(snapshot(&old, &before), snapshot(&new, &after));
    }
    #[test]
    fn full_builtin_catalog_meta_only_change_has_one_boundary_then_reuse() {
        let base = tools::ToolExecutor::with_standard_tools(
            std::sync::Arc::new(event_bus::EventBus::new(4)),
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        let raw = super::super::tool_calls::standard_tool_specs(&base);
        let mut old = raw.clone();
        old.iter_mut()
            .find(|s| s.name == "wait")
            .unwrap()
            .description
            .push_str(" predecessor wait schema");
        let before = snapshot(&base, &old);
        let after = snapshot(&base, &raw);
        let changed = before
            .builtins
            .iter()
            .zip(&after.builtins)
            .filter(|(l, r)| l != r)
            .map(|(l, _)| l["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(changed, ["wait"]);
        boundary(before, after);
    }
    #[test]
    fn live_and_preview_catalog_options_preserve_legacy_names_schema_description_and_order() {
        let bus = std::sync::Arc::new(event_bus::EventBus::new(4));
        let executor = tools::ToolExecutor::with_standard_tools(
            bus,
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        let settings = config::RunJsConfig::default();
        for role in [crate::Role::Orchestrator, crate::Role::Worker] {
            for child in [false, true] {
                for enabled in [false, true] {
                    let policy = crate::ExecutionPolicy::for_role(role);
                    let base = super::super::tool_calls::standard_tool_specs(&executor);
                    let mut legacy =
                        visible_tool_specs(base.clone(), &policy, enabled, child, enabled);
                    retain_backed_tools(&mut legacy, enabled);
                    super::super::run_js_host::apply_run_js_specs(&mut legacy, enabled, &settings);
                    append_subagent_context_note(&mut legacy, crate::ModelFamily::Gpt5);
                    assert_eq!(
                        compose(
                            base,
                            &policy,
                            CatalogOptions {
                                skills: enabled,
                                child,
                                web: enabled,
                                artifacts: enabled,
                                run_js: enabled,
                                run_js_settings: &settings,
                                family: crate::ModelFamily::Gpt5
                            }
                        ),
                        legacy
                    );
                }
            }
        }
    }
    #[test]
    fn frozen_mcp_preview_matches_live_registration_pipeline_without_discovery_or_events() {
        use tools::mcp::{
            McpServerTools, McpToolTransport,
            types::{ApprovalFingerprint, FrozenMcpSnapshot, McpToolDefinition},
        };
        let snapshot = FrozenMcpSnapshot::from_servers([McpServerTools {
            server_id: "offline-server".into(),
            transport: McpToolTransport::Http,
            approval_fingerprint: ApprovalFingerprint::from_projection(b"approved-fixture"),
            definitions: vec![McpToolDefinition {
                name: "lookup".into(),
                description: Some("Frozen, not discovered".into()),
                input_schema: serde_json::from_value(
                    serde_json::json!({"type":"object","properties":{"q":{"type":"string"}}}),
                )
                .unwrap(),
            }],
        }])
        .unwrap();
        let bus = std::sync::Arc::new(event_bus::EventBus::new(4));
        let mut events = bus.subscribe();
        let base = tools::ToolExecutor::with_standard_tools(
            bus.clone(),
            std::sync::Arc::new(sandbox::DirectSandbox::new_unchecked()),
        );
        let original = super::super::tool_calls::standard_tool_specs(&base);
        // Actual mcp_setup uses this frozen-only API even when disconnected.
        let lease = crate::mcp::RunMcpLease::restore_snapshot(snapshot.clone());
        let live = lease.register_executor(&base, bus).unwrap();
        let live_specs = super::super::tool_calls::standard_tool_specs(&live);
        let preview = restored_specs(&base, Some(&snapshot)).unwrap();
        assert_eq!(preview, live_specs);
        assert_eq!(restored_specs(&base, None).unwrap(), original);
        assert_eq!(
            super::super::tool_calls::standard_tool_specs(&base),
            original,
            "preview never mutates the base executor"
        );
        let settings = config::RunJsConfig::default();
        for role in [crate::Role::Orchestrator, crate::Role::Worker] {
            let mut policy = crate::ExecutionPolicy::for_role(role);
            allow_saved_mcp_tools(&mut policy, &snapshot);
            for child in [false, true] {
                for enabled in [false, true] {
                    let mut legacy =
                        visible_tool_specs(live_specs.clone(), &policy, true, child, true);
                    retain_backed_tools(&mut legacy, true);
                    super::super::run_js_host::apply_run_js_specs(&mut legacy, enabled, &settings);
                    append_subagent_context_note(&mut legacy, crate::ModelFamily::Gpt5);
                    let composed = compose(
                        preview.clone(),
                        &policy,
                        CatalogOptions {
                            skills: true,
                            child,
                            web: true,
                            artifacts: true,
                            run_js: enabled,
                            run_js_settings: &settings,
                            family: crate::ModelFamily::Gpt5,
                        },
                    );
                    assert_eq!(composed, legacy);
                    assert!(
                        composed
                            .iter()
                            .any(|tool| tool.name == snapshot.tools[0].public_name)
                    );
                }
            }
        }
        use futures_util::FutureExt;
        assert!(
            events.recv().now_or_never().is_none(),
            "inert preview/registration publishes no event"
        );
    }
}
