use providers::provider::codex::tokens::{CodexTokenStore, parse_jwt_claims};
use providers::{ProviderAuth, ProviderError};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::RoutedModel;

impl RoutedModel {
    /// Verify every composed profile, including cross-role fallbacks and explicit selections.
    /// Results, including failures, are retained until this composition is replaced.
    pub async fn verify_candidates(&self) -> BTreeSet<String> {
        self.verification
            .get_or_init(|| async {
                let mut endpoints = BTreeMap::new();
                let mut profiles = BTreeMap::new();
                let mut codex_version: Option<providers::CodexCatalogVersion> = None;
                for (name, provider) in &self.providers {
                    let (auth, account) = match self.verification_auth(provider) {
                        Ok(auth) => auth,
                        Err(error) => {
                            profiles.insert(name.clone(), Err(error));
                            continue;
                        }
                    };
                    let key = (
                        provider.profile.base_url.trim_end_matches('/').to_owned(),
                        auth.api_key.clone(),
                        account.clone(),
                    );
                    let result = match endpoints.get(&key) {
                        Some(result) => result,
                        None => {
                            let result = match account {
                                Some(account) => {
                                    // Resolve once, only when a Codex catalog is actually needed.
                                    let catalog_version = match &codex_version {
                                        Some(version) => version,
                                        None => codex_version
                                            .insert(self.codex_version_resolver.resolve().await),
                                    };
                                    providers::list_codex_models(
                                        &key.0,
                                        &auth,
                                        &account,
                                        &catalog_version.version,
                                    )
                                    .await
                                    .map(|models| {
                                        models.into_iter().map(|model| model.slug).collect()
                                    })
                                }
                                None => providers::list_models(&key.0, &auth).await,
                            }
                            .map_err(|error| match error {
                                ProviderError::Http { status, .. } => ProviderError::Http {
                                    status,
                                    body: "catalog verification failed".into(),
                                },
                                other => other,
                            });
                            endpoints.entry(key).or_insert(result)
                        }
                    };
                    profiles.insert(name.clone(), result.clone());
                }
                profiles
            })
            .await
            .iter()
            .filter(|(_, result)| result.is_ok())
            .map(|(name, _)| name.clone())
            .collect()
    }

    pub(super) async fn admit_candidates(
        &self,
        invocation: &crate::AgentInvocationContext,
        role: agents::Role,
    ) -> Result<(), crate::RuntimeError> {
        let selected = match &invocation.model_preference {
            Some(preference) => {
                let provider = self.providers.get(&preference.profile).ok_or_else(|| {
                    self.provider_unavailable(
                        &preference.profile,
                        format!(
                            "selected provider profile `{}` is not configured",
                            preference.profile
                        ),
                        invocation,
                    )
                })?;
                routing::ResolvedRoute {
                    profile: preference.profile.clone(),
                    model_id: preference
                        .model
                        .clone()
                        .unwrap_or_else(|| provider.profile.default_model.clone()),
                }
            }
            None => {
                let binding = self
                    .agents
                    .binding_for(super::role_key(role), invocation.category.as_deref())
                    .map_err(super::model_error)?;
                let logical = model::LogicalModelId::from(binding.logical_model);
                self.resolve(&invocation.run_id, &logical, false)
                    .map_err(|error| {
                        super::route_resolution_error(error, role, invocation, &logical)
                    })?
            }
        };
        self.verify_candidates().await;
        self.verify_route(&selected, invocation)?;
        for route in &self.admission_routes {
            self.verify_route(route, invocation)?;
        }
        Ok(())
    }

    fn verify_route(
        &self,
        route: &routing::ResolvedRoute,
        invocation: &crate::AgentInvocationContext,
    ) -> Result<(), crate::RuntimeError> {
        let (base, _) = config::types::provider::parse_model_speed(&route.model_id);
        if let Some(provider) = self.providers.get(&route.profile)
            && !provider.profile.models.is_empty()
            && !provider.profile.models.contains(&route.model_id)
        {
            return Err(crate::RuntimeError::Model {
                reason: format!(
                    "model `{}` is not listed for profile `{}`",
                    route.model_id, route.profile
                ),
            });
        }
        let failure = match self
            .verification
            .get()
            .and_then(|results| results.get(&route.profile))
        {
            Some(Ok(models)) if models.iter().any(|model| model == base) => return Ok(()),
            Some(Ok(_)) => "model is not advertised by provider".to_owned(),
            Some(Err(error)) => error.to_string(),
            None => "profile was not verified".to_owned(),
        };
        let detail = format!(
            "profile={} model={}: {failure}",
            route.profile, route.model_id
        );
        Err(self.provider_unavailable(&route.profile, detail, invocation))
    }

    fn provider_unavailable(
        &self,
        profile: &str,
        detail: String,
        invocation: &crate::AgentInvocationContext,
    ) -> crate::RuntimeError {
        let detail = match self.providers.get(profile) {
            Some(provider) if !provider.auth.api_key.is_empty() => {
                detail.replace(&provider.auth.api_key, "***")
            }
            Some(_) | None => detail,
        };
        let detail: String = detail.chars().take(500).collect();
        if let Some(bus) = &self.event_bus {
            bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                source: "runtime::compose".into(),
                severity: event_bus::DiagnosticSeverity::Error,
                code: event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE.into(),
                detail: detail.clone(),
                run_id: Some(invocation.run_id.clone()),
                thread_id: None,
                call_id: None,
            }));
        }
        crate::RuntimeError::Model { reason: detail }
    }

    fn verification_auth(
        &self,
        provider: &routing::ComposedProvider,
    ) -> Result<(ProviderAuth, Option<String>), ProviderError> {
        match provider.profile.provider_type {
            model::ProviderType::OpenAiCodex => {}
            model::ProviderType::OpenAiCompatible
            | model::ProviderType::KimiSubscription
            | model::ProviderType::Anthropic
            | model::ProviderType::AnthropicSubscription
            | model::ProviderType::OpenAi
            | model::ProviderType::GithubCopilot
            | model::ProviderType::Openrouter => return Ok((provider.auth.clone(), None)),
        }
        let store = self.credential_store.as_ref().ok_or_else(|| {
            ProviderError::Request("Codex credential store is unavailable".into())
        })?;
        let account = match &provider.profile.credential {
            routing::CredentialRef::Keyring { account, .. } => account,
            routing::CredentialRef::Env { .. } => {
                return Err(ProviderError::Request(
                    "Codex requires keyring credentials".into(),
                ));
            }
        };
        let tokens =
            routing::factory::CredentialStoreTokenStore::new(Arc::clone(store), account.clone())
                .load()?
                .ok_or_else(|| ProviderError::Request("Codex credentials are missing".into()))?;
        let claims = parse_jwt_claims(&tokens.id_token)?;
        Ok((
            ProviderAuth::new(tokens.access_token),
            Some(claims.chatgpt_account_id),
        ))
    }
}
