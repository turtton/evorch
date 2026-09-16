use model::{Capability, CapabilitySupport, ModelCatalog, ProviderType};

#[test]
fn kimi_models_support_tools_when_using_builtin_catalog() {
    // Given: the offline catalog and the two subscription model identifiers.
    let catalog = ModelCatalog::builtin();
    for (model_id, reasoning) in [("kimi-for-coding", false), ("kimi-k2-thinking", true)] {
        // When: looking up the built-in model.
        let entry = catalog.get(model_id).expect("Kimi builtin entry exists");
        // Then: tool support and the model-specific reasoning declaration are available.
        assert_eq!(entry.provider, ProviderType::KimiSubscription);
        assert_eq!(
            catalog.capability_support(model_id, Capability::ToolCalling),
            CapabilitySupport::Supported
        );
        assert_eq!(entry.capabilities.reasoning, reasoning);
        assert_eq!(entry.context_window, 256_000);
        assert_eq!(entry.max_output_tokens, 32_000);
    }
}
