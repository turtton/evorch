use providers::ProviderError;
use std::collections::{BTreeMap, BTreeSet};

use super::RoutedModel;

enum RouteFailure {
    Unlisted(String),
    Unavailable(String),
}

impl RoutedModel {
    /// Verify every composed profile, including cross-role fallbacks and explicit selections.
    /// Results, including failures, are retained until this composition is replaced.
    /// Degraded absence fails early and requires a new composition; refreshing a
    /// running client's version would invalidate its prompt-cache affinity.
    pub async fn verify_candidates(&self) -> BTreeSet<String> {
        self.verification
            .get_or_init(|| async {
                let mut endpoints = BTreeMap::new();
                let mut profiles = BTreeMap::new();
                for (name, provider) in &self.providers {
                    match provider.client.list_models(&provider.auth).await {
                        Ok(Some(models)) => {
                            profiles.insert(name.clone(), Ok(models));
                            continue;
                        }
                        Err(error) => {
                            profiles.insert(name.clone(), Err(error));
                            continue;
                        }
                        Ok(None) => {}
                    }
                    let key = (
                        provider.profile.base_url.trim_end_matches('/').to_owned(),
                        provider.auth.api_key.clone(),
                    );
                    let result = match endpoints.get(&key) {
                        Some(result) => result,
                        None => {
                            let result = providers::list_models(&key.0, &provider.auth)
                                .await
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

    /// Admits a run when its selected route verifies. Other configured routes only
    /// lose their place in fallback, so one broken profile cannot stop every run.
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
                let (_, choice) = self.resolve_binding(
                    &invocation.run_id,
                    role,
                    invocation.category.as_deref(),
                    false,
                )?;
                choice.chain.member(choice.position).target.clone()
            }
        };
        self.verify_candidates().await;
        match self.route_failure(&selected) {
            None => {}
            Some(RouteFailure::Unlisted(reason)) => {
                return Err(crate::RuntimeError::Model { reason });
            }
            Some(RouteFailure::Unavailable(detail)) => {
                return Err(self.provider_unavailable(&selected.profile, detail, invocation));
            }
        }
        self.report_unverified_routes(&selected, invocation);
        Ok(())
    }

    fn route_failure(&self, route: &routing::ResolvedRoute) -> Option<RouteFailure> {
        let (base, _) = config::types::provider::parse_model_speed(&route.model_id);
        if let Some(provider) = self.providers.get(&route.profile)
            && !provider.profile.models.is_empty()
            && !provider.profile.models.contains(&route.model_id)
        {
            return Some(RouteFailure::Unlisted(format!(
                "model `{}` is not listed for profile `{}`",
                route.model_id, route.profile
            )));
        }
        let failure = match self
            .verification
            .get()
            .and_then(|results| results.get(&route.profile))
        {
            Some(Ok(models)) if models.iter().any(|model| model == base) => return None,
            Some(Ok(_)) => "model is not advertised by provider".to_owned(),
            Some(Err(error)) => error.to_string(),
            None => "profile was not verified".to_owned(),
        };
        Some(RouteFailure::Unavailable(format!(
            "profile={} model={}: {failure}",
            route.profile, route.model_id
        )))
    }

    /// Resolve one preflight binding using the same router and chat-chain checks
    /// as admission. A scratch affinity keeps this read-only with respect to any
    /// already-running session. Every fallback must resolve, not just the head.
    pub fn preflight_chain(
        &self,
        key: &str,
        entries: &[config::ChainEntryConfig],
    ) -> Result<Vec<routing::ResolvedRoute>, crate::RuntimeError> {
        let error = |error| crate::RuntimeError::Model {
            reason: format!("preflight binding `{key}`: {error}"),
        };
        let chain = self.router.chain(key, entries).map_err(error)?;
        self.ensure_chat_chain(&chain)?;
        self.router
            .resolve_chain(
                &mut routing::SessionAffinity::default(),
                "preflight",
                &chain,
            )
            .map_err(error)?;
        Ok(chain
            .members()
            .iter()
            .map(|member| member.target.clone())
            .collect())
    }

    /// Evaluation gate: every route must be listed and advertised by a fetched
    /// catalog. This sends no inference requests.
    pub async fn preflight_routes(
        &self,
        routes: &[routing::ResolvedRoute],
    ) -> Result<(), crate::RuntimeError> {
        self.verify_candidates().await;
        for route in routes {
            if let Some(RouteFailure::Unlisted(reason) | RouteFailure::Unavailable(reason)) =
                self.route_failure(route)
            {
                return Err(crate::RuntimeError::Model {
                    reason: self.scrub(&route.profile, &reason),
                });
            }
        }
        Ok(())
    }

    /// Warns once per composition about configured routes that fallback will pass over.
    fn report_unverified_routes(
        &self,
        selected: &routing::ResolvedRoute,
        invocation: &crate::AgentInvocationContext,
    ) {
        let failures: BTreeSet<String> = self
            .admission_routes
            .iter()
            .filter(|route| *route != selected)
            .filter_map(|route| match self.route_failure(route)? {
                RouteFailure::Unlisted(detail) | RouteFailure::Unavailable(detail) => {
                    Some(self.scrub(&route.profile, &detail))
                }
            })
            .collect();
        if failures.is_empty()
            || self
                .unverified_reported
                .swap(true, std::sync::atomic::Ordering::Relaxed)
        {
            return;
        }
        let detail = failures.into_iter().collect::<Vec<_>>().join("; ");
        self.emit_unavailable(
            event_bus::DiagnosticSeverity::Warning,
            detail.chars().take(500).collect(),
            invocation,
        );
    }

    fn provider_unavailable(
        &self,
        profile: &str,
        detail: String,
        invocation: &crate::AgentInvocationContext,
    ) -> crate::RuntimeError {
        let detail: String = self.scrub(profile, &detail).chars().take(500).collect();
        self.emit_unavailable(
            event_bus::DiagnosticSeverity::Error,
            detail.clone(),
            invocation,
        );
        crate::RuntimeError::Model { reason: detail }
    }

    fn scrub(&self, profile: &str, detail: &str) -> String {
        match self.providers.get(profile) {
            Some(provider) if !provider.auth.api_key.is_empty() => {
                detail.replace(&provider.auth.api_key, "***")
            }
            Some(_) | None => detail.to_owned(),
        }
    }

    fn emit_unavailable(
        &self,
        severity: event_bus::DiagnosticSeverity,
        detail: String,
        invocation: &crate::AgentInvocationContext,
    ) {
        if let Some(bus) = &self.event_bus {
            bus.emit(event_bus::Event::new(event_bus::DiagnosticEvent {
                source: "runtime::compose".into(),
                severity,
                code: event_bus::event::diagnostic_codes::PROVIDER_UNAVAILABLE.into(),
                detail,
                run_id: Some(invocation.run_id.clone()),
                thread_id: None,
                call_id: None,
            }));
        }
    }
}
