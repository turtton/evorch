use model::{
    Capability, CapabilitySupport, CatalogCapabilities, CatalogEntry, ModelCapabilities,
    ModelCatalog,
};

#[test]
fn legacy_catalog_json_preserves_explicit_support() {
    // Given: a persisted entry with the original bool-only shape.
    let json = r#"{
        "model_id":"legacy", "provider":"openai", "context_window":128000,
        "max_output_tokens":4096,
        "capabilities":{"tool_calling":true,"reasoning":false,"prompt_cache":true},
        "price":null,"availability":"available","source":"models-dev",
        "attributes_confirmed":true
    }"#;
    // When: loading the existing catalog entry and querying its canonical view.
    let entry: CatalogEntry = serde_json::from_str(json).expect("legacy entry parses");
    let capabilities = entry.model_capabilities();
    // Then: explicit bools survive and new dimensions remain unknown.
    assert_eq!(capabilities.tool_calling, CapabilitySupport::Supported);
    assert_eq!(capabilities.reasoning, CapabilitySupport::Unsupported);
    assert_eq!(capabilities.prompt_cache, CapabilitySupport::Supported);
    assert_eq!(capabilities.streaming, CapabilitySupport::Unknown);
    assert_eq!(capabilities.vision, CapabilitySupport::Unknown);
    let encoded = serde_json::to_value(&entry).expect("entry serializes");
    assert_eq!(encoded["capabilities"]["tool_calling"], true);
    assert_eq!(encoded["capabilities"]["reasoning"], false);
}

#[test]
fn canonical_json_distinguishes_missing_from_explicit_false() {
    // Given: a partial legacy capabilities object.
    let json = r#"{"tool_calling":true,"reasoning":false}"#;
    // When: parsing directly into the canonical type.
    let capabilities: ModelCapabilities = serde_json::from_str(json).expect("bools parse");
    // Then: only declared false is unsupported.
    assert_eq!(
        capabilities,
        ModelCapabilities {
            tool_calling: CapabilitySupport::Supported,
            reasoning: CapabilitySupport::Unsupported,
            ..ModelCapabilities::default()
        }
    );
}

#[test]
fn legacy_struct_converts_without_inventing_dimensions() {
    // Given: existing callers construct bool capabilities.
    let legacy = CatalogCapabilities {
        tool_calling: true,
        reasoning: false,
        prompt_cache: false,
    };
    // When: converting without changing the legacy source API.
    let capabilities = ModelCapabilities::from(legacy);
    // Then: the three explicit declarations convert, the rest stay unknown.
    assert_eq!(
        capabilities,
        ModelCapabilities {
            tool_calling: CapabilitySupport::Supported,
            reasoning: CapabilitySupport::Unsupported,
            prompt_cache: CapabilitySupport::Unsupported,
            ..ModelCapabilities::default()
        }
    );
}

#[test]
fn discovered_and_missing_models_safely_degrade() {
    // Given: a catalog populated with a newly discovered ID.
    let mut catalog = ModelCatalog::builtin();
    catalog.merge_discovered(vec!["discovered".into()]);
    // When: resolving the canonical capabilities.
    let entry = catalog.get("discovered").expect("discovered entry exists");
    let capabilities = entry.model_capabilities();
    // Then: placeholders and absent IDs never acquire support.
    assert_eq!(capabilities, ModelCapabilities::default());
    assert!(!entry.attributes_confirmed);
    for dimension in [
        Capability::ToolCalling,
        Capability::Reasoning,
        Capability::Streaming,
        Capability::Vision,
        Capability::PromptCache,
    ] {
        assert_eq!(
            catalog.capability_support("discovered", dimension),
            CapabilitySupport::Unknown
        );
        assert_eq!(
            catalog.capability_support("missing", dimension),
            CapabilitySupport::Unknown
        );
        assert!(!catalog.supports("discovered", dimension));
        assert!(!catalog.supports("missing", dimension));
    }
}

#[test]
fn catalog_sources_leave_vision_unknown() {
    // Given: builtin, external, and discovered catalog sources.
    let mut catalog = ModelCatalog::builtin();
    let mut external = catalog.get("gpt-4o").expect("builtin exists").clone();
    external.model_id = "external".into();
    catalog.merge_models_dev(vec![external]);
    catalog.merge_discovered(vec!["discovered".into()]);
    // When: reading each canonical view.
    let views: Vec<_> = catalog
        .entries()
        .values()
        .map(CatalogEntry::model_capabilities)
        .collect();
    // Then: no catalog source infers vision support.
    assert!(
        views
            .iter()
            .all(|view| view.vision == CapabilitySupport::Unknown)
    );
}

#[test]
fn all_support_states_round_trip_and_safely_degrade() {
    // Given: every support state and its wire representation.
    for (state, wire, supported) in [
        (CapabilitySupport::Unknown, "unknown", false),
        (CapabilitySupport::Unsupported, "unsupported", false),
        (CapabilitySupport::Supported, "supported", true),
    ] {
        let original = ModelCapabilities {
            tool_calling: state,
            reasoning: state,
            streaming: state,
            vision: state,
            prompt_cache: state,
        };
        // When: serializing and deserializing the canonical type.
        let encoded = serde_json::to_value(original).expect("serialize");
        let decoded: ModelCapabilities =
            serde_json::from_value(encoded.clone()).expect("deserialize");
        // Then: state is retained and only Supported enables each dimension.
        assert_eq!(encoded["vision"], wire);
        assert_eq!(decoded, original);
        for dimension in [
            Capability::ToolCalling,
            Capability::Reasoning,
            Capability::Streaming,
            Capability::Vision,
            Capability::PromptCache,
        ] {
            assert_eq!(decoded.support(dimension), state);
            assert_eq!(decoded.is_supported(dimension), supported);
        }
    }
}
