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
                                    providers::list_codex_models(&key.0, &auth, &account)
                                        .await
                                        .map(|_| ())
                                }
                                None => providers::verify_connectivity(&key.0, &auth).await,
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
