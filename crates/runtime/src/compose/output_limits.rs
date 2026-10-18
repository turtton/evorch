use super::RoutedModel;

impl RoutedModel {
    pub(super) async fn metadata_catalog(&self) -> Option<&catalog::ModelCatalog> {
        self.metadata_catalog
            .get_or_init(|| async {
                if self.provider_configs.is_empty() {
                    None
                } else {
                    crate::model_resolve::load_catalog(&catalog::default_cache_dir()).await
                }
            })
            .await
            .as_ref()
    }

    pub(super) async fn output_limit(
        &self,
        route: &routing::ResolvedRoute,
        explicit: Option<u32>,
    ) -> Option<u64> {
        if let Some(value) = explicit {
            return Some(u64::from(value));
        }
        let protocol = self.providers.get(&route.profile)?.profile.api_protocol;
        if !matches!(
            protocol,
            model::ApiProtocol::AnthropicMessages | model::ApiProtocol::OpenAiCompletions
        ) {
            return None;
        }
        let profile = self.provider_configs.get(&route.profile)?;
        let (base_model, _) = config::types::provider::parse_model_speed(&route.model_id);
        let entry = profile
            .models
            .iter()
            .filter(|entry| entry.enabled)
            .find(|entry| entry.id == route.model_id)
            .or_else(|| {
                profile
                    .models
                    .iter()
                    .find(|entry| entry.enabled && entry.id == base_model)
            })
            .cloned()
            .unwrap_or_else(|| config::ModelEntryConfig::enabled(base_model));
        let resolved = crate::model_resolve::resolve_model_metadata(
            &entry,
            &self.model_presets,
            None,
            Some(&route.profile),
            Some(profile.provider_type),
        );
        if resolved.max_output_tokens.is_some() {
            return resolved.max_output_tokens;
        }
        crate::model_resolve::resolve_model_metadata(
            &entry,
            &self.model_presets,
            self.metadata_catalog().await,
            Some(&route.profile),
            Some(profile.provider_type),
        )
        .max_output_tokens
    }
}
