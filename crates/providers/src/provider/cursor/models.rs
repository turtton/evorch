//! Account model routes, adapted from OMP's Cursor discovery / resolveCursorWireModel.
use super::{CursorClient, Proto, wire};
use crate::ProviderError;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug)]
pub(super) struct ModelRoute {
    pub model_id: String,
    pub details_id: String,
    pub parameters: Vec<(String, String)>,
    pub max_mode: bool,
    // A discovered variant or effort sibling owns its parameters, including an
    // empty list for a no-reasoning sibling. Generic request effort cannot alter it.
    fixed_parameters: bool,
}
impl ModelRoute {
    fn legacy(id: &str, max_mode: bool) -> Self {
        let mut route = Self {
            model_id: id.into(),
            details_id: id.into(),
            parameters: Vec::new(),
            max_mode,
            fixed_parameters: false,
        };
        // Cursor's OpenAI effort slugs belong only in modelDetails. Preserve the
        // fast price lane while moving the effort into requestedModel.parameters.
        let (stem, lane) = id.strip_suffix("-fast").map_or((id, ""), |s| (s, "-fast"));
        if let Some((base, effort)) = stem.rsplit_once('-')
            && openai_family(base)
            && matches!(
                effort,
                "none" | "off" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max"
            )
        {
            route.model_id = format!("{base}{lane}");
            route.fixed_parameters = true;
            if !matches!(effort, "none" | "off") {
                route.parameters.push(("reasoning".into(), effort.into()));
            }
        } else if id == "composer-2.5" {
            // Cursor defaults this bare ID to Fast; OMP pins the Standard lane.
            route.parameters.push(("fast".into(), "false".into()));
            route.fixed_parameters = true;
        } else if !openai_family(stem) {
            // Other legacy IDs pass through unchanged, as in OMP resolveCursorWireModel:
            // their name may already fix an effort (`claude-…-xhigh`), and a guessed
            // `reasoning` parameter would contradict it.
            route.fixed_parameters = true;
        }
        route
    }
    /// The effort this route sends, from its `reasoning` parameter.
    pub(super) fn reasoning(&self) -> Option<&str> {
        self.parameters
            .iter()
            .find(|(id, _)| id == "reasoning")
            .map(|(_, value)| value.as_str())
    }
    fn with_effort(mut self, effort: Option<&str>) -> Self {
        if !self.fixed_parameters
            && let Some(effort) = effort
        {
            self.parameters.push(("reasoning".into(), effort.into()));
        }
        self
    }
}

/// Cursor's OpenAI families, the only legacy IDs that carry effort as a parameter.
fn openai_family(base: &str) -> bool {
    base.starts_with("gpt-")
        || ["o1", "o3", "o4"]
            .iter()
            .any(|p| base == *p || base.starts_with(&format!("{p}-")))
}

impl CursorClient {
    pub(super) async fn discover_models(
        &self,
        token: &str,
    ) -> Result<HashMap<String, ModelRoute>, ProviderError> {
        let (usable, available) = tokio::join!(
            self.catalog(
                token,
                "/agent.v1.AgentService/GetUsableModels",
                Proto::new()
            ),
            self.catalog(
                token,
                "/aiserver.v1.AiService/AvailableModels",
                Proto::new()
                    .integer(2, 1)
                    .integer(5, 1)
                    .integer(7, 1)
                    .integer(12, 1)
            )
        );
        for response in [&usable, &available] {
            if let Err(error) = response
                && matches!(error.status(), Some(401 | 403))
            {
                return Err(super::status_error(
                    error.status().unwrap(),
                    "Cursor catalog authorization failed",
                ));
            }
        }
        let mut routes = HashMap::new();
        if let Ok(bytes) = &usable {
            for row in messages(bytes, 1)? {
                let id = wire::string(row, 1)?.trim().to_owned();
                if !id.is_empty() {
                    routes.insert(
                        id.clone(),
                        ModelRoute::legacy(&id, wire::integer(row, 7)? == Some(1)),
                    );
                }
            }
        }
        match available {
            Ok(bytes) => add_rich_routes(&mut routes, &bytes)?,
            Err(error) if usable.is_err() => return Err(error),
            Err(_) => {}
        }
        Ok(routes)
    }
    /// Resolves `id` to its wire route, with why it is a guessed legacy route when the
    /// account catalog did not provide it.
    pub(super) async fn model_route(
        &self,
        token: &str,
        id: &str,
        effort: Option<&str>,
    ) -> Result<(ModelRoute, Option<RouteFallback>), ProviderError> {
        let mut routes = self.model_routes.lock().await;
        if !routes.contains_key(id) {
            match self.discover_models(token).await {
                Ok(discovered) => routes.extend(discovered),
                Err(error) if matches!(error.status(), Some(401 | 403)) => return Err(error),
                // Older/custom endpoints may expose only RunSSE. The guess is not cached,
                // so the next conversation asks the catalog again.
                Err(error) => {
                    let route = ModelRoute::legacy(id, false).with_effort(effort);
                    return Ok((
                        route,
                        Some(RouteFallback::DiscoveryFailed(failure_kind(&error))),
                    ));
                }
            }
        }
        let fallback = (!routes.contains_key(id)).then_some(RouteFallback::NotListed);
        let route = routes
            .entry(id.into())
            .or_insert_with(|| ModelRoute::legacy(id, false))
            .clone()
            .with_effort(effort);
        Ok((route, fallback))
    }
}

/// Why a model was sent on a legacy route guessed from its ID alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum RouteFallback {
    /// The account catalog answered without this ID.
    NotListed,
    /// The account catalog could not be read; holds the failure kind.
    DiscoveryFailed(&'static str),
}

impl std::fmt::Display for RouteFallback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotListed => f.write_str("model not listed in the account catalog"),
            Self::DiscoveryFailed(kind) => write!(f, "account catalog unavailable ({kind})"),
        }
    }
}

/// The failure class only: catalog errors may carry server text.
fn failure_kind(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::Http { status, .. } if *status >= 500 => "server error",
        ProviderError::Http { .. } => "http error",
        ProviderError::RateLimited { .. } => "rate limited",
        ProviderError::Timeout => "timeout",
        ProviderError::Transport { .. } => "transport",
        ProviderError::InvalidSse { .. } | ProviderError::InvalidJson { .. } => "invalid response",
        ProviderError::RetriesExhausted { last, .. } => failure_kind(last),
        ProviderError::Request(_) | ProviderError::ResponsesScopeChanged { .. } => "request",
    }
}

fn messages(bytes: &[u8], number: u32) -> Result<Vec<&[u8]>, ProviderError> {
    Ok(wire::fields(bytes)?
        .into_iter()
        .filter(|f| f.number == number && f.wire == 2)
        .map(|f| f.data)
        .collect())
}
fn strings(bytes: &[u8], number: u32) -> Result<Vec<String>, ProviderError> {
    messages(bytes, number)?
        .into_iter()
        .map(|b| {
            std::str::from_utf8(b)
                .map(|s| s.trim().to_owned())
                .map_err(|_| ProviderError::Request("Malformed Cursor model identifier".into()))
        })
        .collect()
}
fn add_rich_routes(
    routes: &mut HashMap<String, ModelRoute>,
    bytes: &[u8],
) -> Result<(), ProviderError> {
    // A nonempty usable roster is the entitlement boundary; rich data enriches
    // those routes even when GetUsableModels succeeded. Empty usable falls back.
    let usable: HashSet<_> = routes.keys().cloned().collect();
    let mut rich_ids = HashSet::new();
    for details in messages(bytes, 2)? {
        if wire::integer(details, 6)? == Some(2)
            || wire::integer(details, 35)? == Some(1)
            || wire::integer(details, 5)? == Some(0)
            || wire::integer(details, 4)? == Some(1)
            || wire::integer(details, 46)? == Some(1)
        {
            continue;
        }
        // server_model_name is not requestedModel.modelId: the parameterized
        // route is always the rich ModelDetails.name (OMP discovery contract).
        let base = wire::string(details, 1)?.trim().to_owned();
        if base.is_empty() {
            continue;
        }
        let mut aliases = strings(details, 37)?;
        aliases.push(base.clone());
        let variant_base_entitled = usable.is_empty() || aliases.iter().any(|a| usable.contains(a));
        aliases.extend(strings(details, 36)?);
        let base_entitled = usable.is_empty() || aliases.iter().any(|a| usable.contains(a));
        let base_max =
            wire::integer(details, 19)? == Some(0) && wire::integer(details, 14)? == Some(1);
        let variants = messages(details, 30)?;
        if variants.is_empty() {
            if base_entitled {
                let route = ModelRoute {
                    model_id: base.clone(),
                    details_id: base.clone(),
                    parameters: Vec::new(),
                    max_mode: routes.get(&base).map_or(base_max, |r| r.max_mode),
                    fixed_parameters: true,
                };
                routes.insert(base, route.clone());
                for alias in aliases.iter().filter(|id| usable.contains(*id)) {
                    let mut alias_route = route.clone();
                    alias_route.details_id = alias.clone();
                    alias_route.max_mode = routes.get(alias).map_or(base_max, |r| r.max_mode);
                    routes.insert(alias.clone(), alias_route);
                }
            }
            continue;
        }
        let blocked_parameters = blocked_parameters(details)?;
        let mut default_route: Option<(u8, ModelRoute)> = None;
        let mut seen = HashSet::new();
        for (index, variant) in variants.into_iter().enumerate() {
            let parameters = messages(variant, 1)?
                .into_iter()
                .map(|p| {
                    Ok((
                        wire::string(p, 1)?.trim().to_owned(),
                        wire::string(p, 2)?.trim().to_owned(),
                    ))
                })
                .collect::<Result<Vec<_>, ProviderError>>()?
                .into_iter()
                .filter(|(id, _)| !id.is_empty())
                .collect::<Vec<_>>();
            // Cursor may entitle the family while blocking individual parameter
            // values. Filter before picking defaults or advertising any route.
            if parameters.iter().any(|p| blocked_parameters.contains(p)) {
                continue;
            }
            let suffix = parameters
                .iter()
                .map(|(id, value)| format!("{id}={value}"))
                .collect::<Vec<_>>()
                .join(",");
            let slug = wire::string(variant, 11)?.trim().to_owned();
            let representation = wire::string(variant, 9)?.trim().to_owned();
            let mut id = if !slug.is_empty() {
                slug
            } else if !representation.is_empty() {
                representation.clone()
            } else if !suffix.is_empty() {
                format!("{base}@{suffix}")
            } else {
                format!("{base}@variant-{}", index + 1)
            };
            if !variant_base_entitled && !usable.contains(&id) {
                continue;
            }
            let max_mode = wire::integer(variant, 3)?
                .map(|v| v == 1)
                .unwrap_or_else(|| routes.get(&id).map_or(base_max, |r| r.max_mode));
            if !seen.insert((parameters.clone(), max_mode)) {
                continue;
            }
            if rich_ids.contains(&id) {
                id = if !representation.is_empty() {
                    representation
                } else {
                    format!("{id}@{suffix}")
                };
                let root = id.clone();
                let mut duplicate = 2;
                while rich_ids.contains(&id) {
                    id = format!("{root}#{duplicate}");
                    duplicate += 1;
                }
            }
            rich_ids.insert(id.clone());
            let route = ModelRoute {
                model_id: base.clone(),
                details_id: id.clone(),
                parameters,
                max_mode,
                fixed_parameters: true,
            };
            let rank = if wire::integer(variant, 5)? == Some(1) {
                0
            } else if wire::integer(variant, 4)? == Some(1) {
                1
            } else {
                2
            };
            if default_route.as_ref().is_none_or(|(r, _)| rank < *r) {
                default_route = Some((rank, route.clone()));
            }
            routes.insert(id, route);
        }
        // If the usable roster contains the bare/alias row as well as variants,
        // route that advertised row through Cursor's flagged default config.
        if let Some((_, route)) = default_route {
            for alias in aliases
                .iter()
                .filter(|id| usable.contains(*id) && !rich_ids.contains(*id))
            {
                let mut alias_route = route.clone();
                alias_route.details_id = alias.clone();
                routes.insert(alias.clone(), alias_route);
            }
        }
    }
    if usable.is_empty() && routes.is_empty() {
        for id in strings(bytes, 1)?.into_iter().filter(|id| !id.is_empty()) {
            routes.insert(id.clone(), ModelRoute::legacy(&id, false));
        }
    }
    Ok(())
}

fn blocked_parameters(details: &[u8]) -> Result<HashSet<(String, String)>, ProviderError> {
    let mut blocked = HashSet::new();
    for definition in messages(details, 29)? {
        let id = wire::string(definition, 1)?.trim().to_owned();
        let Some(parameter_type) = wire::nested(definition, 4)? else {
            continue;
        };
        // Boolean and enum values have distinct blocked_by_admin_allowlist tags.
        for (kind, blocked_field) in [(1, 6), (2, 4)] {
            if let Some(values) = wire::nested(parameter_type, kind)? {
                for value in messages(values, 1)? {
                    if wire::integer(value, blocked_field)? == Some(1) {
                        blocked.insert((id.clone(), wire::string(value, 1)?.trim().to_owned()));
                    }
                }
            }
        }
    }
    Ok(blocked)
}
