use model::{Capability, CapabilitySupport, ModelCatalog};

#[test]
fn discovered_kimi_models_do_not_inherit_hardcoded_metadata() {
    let mut catalog = ModelCatalog::new();
    catalog.merge_discovered(vec!["kimi-for-coding".into(), "kimi-k2-thinking".into()]);
    for model_id in ["kimi-for-coding", "kimi-k2-thinking"] {
        let entry = catalog.get(model_id).expect("provider-discovered model");
        assert_eq!(entry.context_window, 0);
        assert_eq!(entry.max_output_tokens, 0);
        assert!(!entry.attributes_confirmed);
        assert_eq!(
            catalog.capability_support(model_id, Capability::ToolCalling),
            CapabilitySupport::Unknown
        );
    }
}
