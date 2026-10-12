//! ロールのチェーンをプロバイダプロファイルと実モデル ID へ解決するルーターです。

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::{
    Chain, ChainMember, ProviderHealth, ProviderProfile, RoutePin, RoutingError, SessionAffinity,
};
use model::{Capability, CapabilitySupport, LogicalModelId, ModelCatalog};

/// 解決済みのルート。
///
/// どのプロバイダプロファイルで、どの実モデル ID を使用するかを表します。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRoute {
    /// 使用するプロバイダプロファイル名。
    pub profile: String,
    /// プロバイダ上の実モデル ID。
    pub model_id: String,
}

/// ルーティング設定・プロバイダプロファイル・モデルカタログを用いて
/// ロールのチェーンを具体ルートへ解決します。
#[derive(Clone)]
pub struct Router {
    /// プロファイル名をキーとした検証済みプロバイダプロファイル。
    profiles: BTreeMap<String, ProviderProfile>,
    /// route 名から候補リスト (宣言順) へのルートテーブル。
    routes: BTreeMap<String, Vec<config::RouteCandidateConfig>>,
    /// 利用可否の判定に使用するモデルカタログ。
    catalog: ModelCatalog,
    required_capabilities: Vec<Capability>,
    /// クールダウン中の候補を外すための共有状態。未接続なら外さない。
    health: Option<Arc<ProviderHealth>>,
}

impl std::fmt::Debug for Router {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Router")
            .field("profiles", &self.profiles)
            .field("routes", &self.routes)
            .field("catalog", &self.catalog)
            .field("required_capabilities", &self.required_capabilities)
            .field("has_health", &self.health.is_some())
            .finish()
    }
}

impl PartialEq for Router {
    /// 等価性はルーティング設定に関わるフィールドのみで判定する。
    /// `health` は実行時の状態であり、等価性には含めない。
    fn eq(&self, other: &Self) -> bool {
        self.profiles == other.profiles
            && self.routes == other.routes
            && self.catalog == other.catalog
            && self.required_capabilities == other.required_capabilities
    }
}

impl Router {
    /// Rejects only explicitly unsupported capabilities during resolution and fallback.
    /// Unconfirmed discovered/config-declared models are assumed to support tool calling;
    /// unknown capability knowledge does not exclude them from routing.
    pub fn requiring_capability(mut self, capability: Capability) -> Self {
        self.required_capabilities.push(capability);
        self
    }

    /// Shares the routing catalog with provider request assembly.
    pub fn catalog(&self) -> &ModelCatalog {
        &self.catalog
    }

    /// メンバーが既知のプロファイルにあり、利用可能で、クールダウン中でないかを返す。
    pub fn is_member_available(&self, target: &ResolvedRoute) -> bool {
        self.profiles.contains_key(&target.profile)
            && self.is_eligible(&target.model_id)
            && !self.health.as_ref().is_some_and(|health| {
                health.is_cooling_down(&target.profile, &target.model_id, std::time::Instant::now())
            })
    }

    fn is_eligible(&self, model_id: &str) -> bool {
        let (base_model_id, _) = config::types::provider::parse_model_speed(model_id);
        self.catalog.is_available(model_id)
            && self.required_capabilities.iter().all(|capability| {
                match self.catalog.capability_support(base_model_id, *capability) {
                    CapabilitySupport::Unsupported => false,
                    CapabilitySupport::Supported | CapabilitySupport::Unknown => true,
                }
            })
    }

    /// 検証済みプロファイル・ルーティング設定・モデルカタログからルーターを構築します。
    ///
    /// 全ルート候補は既知のプロファイルを参照していなければならず、
    /// プロファイル名の重複も許されません。
    ///
    /// # Errors
    /// - プロファイル名が重複している場合、[`RoutingError::InvalidProfile`] を返します。
    /// - ルート候補が未知のプロファイル名を参照している場合、
    ///   [`RoutingError::UnknownProfile`] を返します。
    pub fn new(
        profiles: Vec<ProviderProfile>,
        routing: &config::RoutingConfig,
        catalog: ModelCatalog,
    ) -> Result<Self, RoutingError> {
        let mut profile_map = BTreeMap::new();
        for profile in profiles {
            let name = profile.name.clone();
            if profile_map.insert(name.clone(), profile).is_some() {
                return Err(RoutingError::InvalidProfile {
                    reason: format!("プロファイル名 '{name}' が重複しています"),
                });
            }
        }
        for candidates in routing.routes.values() {
            for candidate in candidates {
                if !profile_map.contains_key(&candidate.profile) {
                    return Err(RoutingError::UnknownProfile(candidate.profile.clone()));
                }
            }
        }
        Ok(Self {
            profiles: profile_map,
            routes: routing.routes.clone(),
            catalog,
            required_capabilities: Vec::new(),
            health: None,
        })
    }

    /// クールダウン状態を接続する。接続した場合、解決とフォールバックは
    /// クールダウン中のメンバーとピンを飛ばす。
    pub fn with_health(mut self, health: Arc<ProviderHealth>) -> Self {
        self.health = Some(health);
        self
    }

    /// チェーンの要素を route の候補へ展開します。
    ///
    /// 候補の具体モデルは `model` 上書き (指定時) またはプロファイルの `default_model` です。
    ///
    /// # Errors
    /// 要素が存在しない route を参照する場合、[`RoutingError::UnknownLogicalModel`] を返します。
    pub fn chain(
        &self,
        key: impl Into<String>,
        entries: &[config::ChainEntryConfig],
    ) -> Result<Chain, RoutingError> {
        let mut members = Vec::new();
        for (entry_index, entry) in entries.iter().enumerate() {
            let candidates = self
                .routes
                .get(&entry.route)
                .ok_or_else(|| RoutingError::UnknownLogicalModel(entry.route.clone()))?;
            for candidate in candidates {
                let Some(profile) = self.profiles.get(&candidate.profile) else {
                    continue; // new() で検証済みのため通常は到達しない
                };
                members.push(ChainMember {
                    entry: entry_index,
                    route: entry.route.clone(),
                    target: ResolvedRoute {
                        profile: profile.name.clone(),
                        model_id: candidate
                            .model
                            .clone()
                            .unwrap_or_else(|| profile.default_model.clone()),
                    },
                    reasoning_effort: entry.reasoning_effort.clone(),
                });
            }
        }
        Ok(Chain::new(key.into(), members))
    }

    /// セッションのアフィニティを考慮してチェーンのメンバーを選び、その位置を返します。
    ///
    /// 解決手順:
    /// 1. ピンがあり、ピンしたメンバー (位置がずれていれば同じプロファイル・具体モデルの
    ///    メンバー) が利用可能なら、再ピンせずにその位置を返します。
    /// 2. ピン先が使えない (クールダウン中・設定から削除済みなど) 場合は、フォールバックと
    ///    同じ順序 ([`Chain`] の走査順) でピンの次から利用可能なメンバーを探します。
    ///    ピンは元に戻しません。
    /// 3. ピンがなければ先頭から利用可能な最初のメンバーを選びます。
    /// 4. 2・3 で選んだメンバーはアフィニティへピンされます。
    ///
    /// # Errors
    /// 利用可能なメンバーが 1 つもない場合、[`RoutingError::NoAvailableCandidate`] を返します。
    pub fn resolve_chain(
        &self,
        affinity: &mut SessionAffinity,
        session_id: &str,
        chain: &Chain,
    ) -> Result<usize, RoutingError> {
        let members = chain.members();
        let order: Vec<usize> = match affinity.pinned(session_id, chain.key()) {
            Some(pin) if !members.is_empty() => {
                let located = members
                    .get(pin.position)
                    .filter(|member| {
                        member.target.profile == pin.profile && member.target.model_id == pin.model
                    })
                    .map(|_| pin.position)
                    .or_else(|| chain.position_of(&pin.profile, &pin.model));
                match located {
                    Some(position) if self.is_member_available(&members[position].target) => {
                        return Ok(position);
                    }
                    Some(position) => chain.scan_order(position),
                    // 設定から消えたメンバーは、元の位置にいたメンバーから走査する。
                    None => {
                        let position = pin.position.min(members.len() - 1);
                        std::iter::once(position)
                            .chain(chain.scan_order(position))
                            .collect()
                    }
                }
            }
            Some(_) | None => (0..members.len()).collect(),
        };
        let position = order
            .into_iter()
            .find(|position| self.is_member_available(&members[*position].target))
            .ok_or_else(|| RoutingError::NoAvailableCandidate(chain.key().to_owned()))?;
        pin(affinity, session_id, chain, position);
        Ok(position)
    }

    /// route 1 つだけのチェーンとして論理モデルを解決します。チェーンのキーは route 名です。
    ///
    /// # Errors
    /// [`Router::chain`] と [`Router::resolve_chain`] のエラーを返します。
    pub fn resolve(
        &self,
        affinity: &mut SessionAffinity,
        session_id: &str,
        logical: &LogicalModelId,
    ) -> Result<ResolvedRoute, RoutingError> {
        let chain = self.chain(
            logical.as_str(),
            &[config::ChainEntryConfig::route(logical.as_str())],
        )?;
        let position = self.resolve_chain(affinity, session_id, &chain)?;
        Ok(chain.member(position).target.clone())
    }
}

/// チェーンの `position` のメンバーをセッションのピンにする。
pub fn pin(affinity: &mut SessionAffinity, session_id: &str, chain: &Chain, position: usize) {
    let target = &chain.member(position).target;
    affinity.pin(
        session_id,
        chain.key(),
        RoutePin {
            position,
            profile: target.profile.clone(),
            model: target.model_id.clone(),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::{ResolvedRoute, Router};
    use crate::profile::ProviderProfile;
    use crate::{FailureKind, RoutePin, RoutingError, SessionAffinity};
    use model::{
        Availability, CatalogCapabilities, CatalogEntry, CatalogSource, LogicalModelId,
        ModelCatalog, ProviderType,
    };

    fn profile(name: &str, default_model: &str) -> ProviderProfile {
        let profile_config = config::ProviderProfileConfig {
            provider_type: config::ProviderTypeConfig::OpenAi,
            api_protocol: config::ApiProtocolConfig::OpenAiResponses,
            base_url: "https://api.example.test".to_string(),
            credential: config::CredentialRefConfig::Env {
                var: "API_KEY".to_string(),
            },
            models: vec![config::types::provider::ModelEntryConfig::enabled(
                default_model,
            )],
            excluded_models: Vec::new(),
            default_model: default_model.to_string(),
        };
        ProviderProfile::try_from((name, &profile_config)).expect("有効な設定は変換できる")
    }

    fn candidate(profile: &str, model: Option<&str>) -> config::RouteCandidateConfig {
        config::RouteCandidateConfig {
            profile: profile.to_string(),
            model: model.map(str::to_string),
            auto: false,
        }
    }

    fn routing_config(
        routes: &[(&str, Vec<config::RouteCandidateConfig>)],
    ) -> config::RoutingConfig {
        config::RoutingConfig {
            cooldown: Default::default(),
            fallback: Default::default(),
            routes: routes
                .iter()
                .map(|(logical, candidates)| (logical.to_string(), candidates.clone()))
                .collect(),
        }
    }

    fn logical(name: &str) -> LogicalModelId {
        LogicalModelId::from(name)
    }

    fn failed_route(profile: &str, model_id: &str) -> ResolvedRoute {
        ResolvedRoute {
            profile: profile.to_string(),
            model_id: model_id.to_string(),
        }
    }

    // merge_models_dev は属性確定フラグを強制的に true へ補正するため、
    // availability の指定だけがテストごとに異なる。属性未確定の項目は
    // merge_discovered が生成するプレースホルダで表現する。
    fn catalog_entry(model_id: &str, availability: Availability) -> CatalogEntry {
        CatalogEntry {
            model_id: model_id.to_string(),
            provider: ProviderType::OpenAi,
            context_window: 64_000,
            max_output_tokens: 8_000,
            capabilities: CatalogCapabilities {
                tool_calling: true,
                reasoning: false,
                prompt_cache: false,
            },
            price: None,
            availability,
            source: CatalogSource::ModelsDev,
            attributes_confirmed: true,
        }
    }

    fn build_catalog(confirmed: &[(&str, Availability)], discovered: &[&str]) -> ModelCatalog {
        let mut catalog = ModelCatalog::new();
        for (model_id, availability) in confirmed {
            catalog.merge_models_dev(vec![catalog_entry(model_id, *availability)]);
        }
        catalog.merge_discovered(
            discovered
                .iter()
                .map(|model_id| model_id.to_string())
                .collect(),
        );
        catalog
    }

    // Given: 後ろの候補にピンしたセッション
    // When: 先頭の候補も利用可能なまま解決する
    // Then: ピン先のルートを再ピンなしで使い、ピンは変化しない
    #[test]
    fn resolve_uses_pinned_profile_when_available() {
        let profiles = vec![
            profile("primary", "model-a"),
            profile("secondary", "model-b"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("secondary", None), candidate("primary", None)],
        )]);
        let catalog = build_catalog(
            &[
                ("model-a", Availability::Available),
                ("model-b", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();
        affinity.pin(
            "session-1",
            "summary",
            RoutePin {
                position: 1,
                profile: "primary".into(),
                model: "model-a".into(),
            },
        );

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("ピン先が利用可能なら解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "primary".to_string(),
                model_id: "model-a".to_string(),
            }
        );
        assert_eq!(
            affinity
                .pinned("session-1", "summary")
                .map(|pin| (pin.profile.as_str(), pin.model.as_str())),
            Some(("primary", "model-a")),
            "既にピン済みのため再ピンされない"
        );
    }

    // Given: ピン済みセッションだが、ピン先プロファイルの default_model が利用不可
    // When: 解決する
    // Then: ピンを無視して宣言済み候補から解決する
    #[test]
    fn resolve_ignores_pin_when_model_unavailable() {
        let profiles = vec![
            profile("primary", "model-pinned"),
            profile("secondary", "model-b"),
        ];
        let routing = routing_config(&[("summary", vec![candidate("secondary", None)])]);
        let catalog = build_catalog(
            &[
                ("model-pinned", Availability::Unavailable),
                ("model-b", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();
        affinity.pin(
            "session-1",
            "summary",
            RoutePin {
                position: 0,
                profile: "primary".into(),
                model: "model-pinned".into(),
            },
        );

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("利用可能な候補から解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "secondary".to_string(),
                model_id: "model-b".to_string(),
            }
        );
    }

    // Given: default と異なる利用可能な override をピンしたセッション
    // When: 同じ論理モデルを再解決する
    // Then: ピンしたプロファイルと override が維持される
    #[test]
    fn resolve_pin_preserves_model_override_when_still_eligible() {
        let profiles = vec![profile("primary", "model-default")];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("primary", Some("model-override"))],
        )]);
        let catalog = build_catalog(
            &[
                ("model-default", Availability::Available),
                ("model-override", Availability::Available),
            ],
            &[],
        );
        let router = Router::new(profiles, &routing, catalog).expect("有効な構成");
        let mut affinity = SessionAffinity::default();
        affinity.pin(
            "session-1",
            "summary",
            RoutePin {
                position: 0,
                profile: "primary".into(),
                model: "model-override".into(),
            },
        );

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("ピンしたモデルで解決できる");

        assert_eq!(resolved, failed_route("primary", "model-override"));
    }

    // Given: ピンした override が利用不可で、同じプロファイルの default は利用可能
    // When: 同じ論理モデルを再解決する
    // Then: 宣言された default 候補を選択し、そのモデルへ再ピンする
    #[test]
    fn resolve_repins_after_pinned_model_becomes_ineligible() {
        let profiles = vec![profile("primary", "model-default")];
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("primary", Some("model-override")),
                candidate("primary", None),
            ],
        )]);
        let catalog = build_catalog(
            &[
                ("model-default", Availability::Available),
                ("model-override", Availability::Unavailable),
            ],
            &[],
        );
        let router = Router::new(profiles, &routing, catalog).expect("有効な構成");
        let mut affinity = SessionAffinity::default();
        affinity.pin(
            "session-1",
            "summary",
            RoutePin {
                position: 0,
                profile: "primary".into(),
                model: "model-override".into(),
            },
        );

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("利用可能な default 候補へ解決できる");

        assert_eq!(resolved, failed_route("primary", "model-default"));
        assert_eq!(
            affinity
                .pinned("session-1", "summary")
                .map(|pin| (pin.profile.as_str(), pin.model.as_str())),
            Some(("primary", "model-default"))
        );
    }

    // Given: どちらも属性確定済みで利用可能な 2 候補を宣言順に並べたルート
    // When: 解決する
    // Then: 宣言順の先頭候補が選ばれる
    #[test]
    fn resolve_follows_candidate_declaration_order() {
        let profiles = vec![
            profile("first", "model-first"),
            profile("second", "model-second"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("first", None), candidate("second", None)],
        )]);
        let catalog = build_catalog(
            &[
                ("model-first", Availability::Available),
                ("model-second", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("宣言順の候補から解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "first".to_string(),
                model_id: "model-first".to_string(),
            }
        );
    }

    // Given: 先頭候補のカタログ項目が属性未確定、次候補が属性確定済みのルート
    // When: 解決する
    // Then: 属性の確定状態に関係なく宣言順先頭の候補が選ばれる
    #[test]
    fn resolve_keeps_declared_order_when_first_candidate_is_unconfirmed() {
        let profiles = vec![
            profile("unconfirmed-first", "model-unconfirmed"),
            profile("confirmed-second", "model-confirmed"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("unconfirmed-first", None),
                candidate("confirmed-second", None),
            ],
        )]);
        let catalog = build_catalog(
            &[("model-confirmed", Availability::Available)],
            &["model-unconfirmed"],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("宣言順の候補から解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "unconfirmed-first".to_string(),
                model_id: "model-unconfirmed".to_string(),
            }
        );
    }

    // Given: 先頭候補が利用不可、次候補が利用可能なルート
    // When: 宣言順に解決する
    // Then: 利用不可の先頭を飛ばして次候補が選ばれる
    #[test]
    fn resolve_skips_ineligible_first_candidate_to_second_declared() {
        let profiles = vec![
            profile("first", "model-first"),
            profile("second", "model-second"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("first", None), candidate("second", None)],
        )]);
        let catalog = build_catalog(
            &[
                ("model-first", Availability::Unavailable),
                ("model-second", Availability::Available),
            ],
            &[],
        );
        let router = Router::new(profiles, &routing, catalog).expect("有効な構成");
        let mut affinity = SessionAffinity::default();

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("次候補から解決できる");

        assert_eq!(resolved, failed_route("second", "model-second"));
    }

    // Given: base が属性確定済みの fast variant を先頭に、別の確定済み候補を後位に宣言したルート
    // When: 解決する
    // Then: fast variant は base の適格性で判定され、宣言順どおり先頭が選ばれる
    #[test]
    fn resolve_treats_fast_variant_as_confirmed_via_base_model() {
        let profiles = vec![
            profile("fast-first", "model-x+fast"),
            profile("confirmed-second", "model-y"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("fast-first", None),
                candidate("confirmed-second", None),
            ],
        )]);
        let catalog = build_catalog(
            &[
                ("model-x", Availability::Available),
                ("model-y", Availability::Available),
            ],
            &["model-x+fast"],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("fast variant を含む候補から解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "fast-first".to_string(),
                model_id: "model-x+fast".to_string(),
            }
        );
    }

    // Given: 属性未確定のカタログ項目しか持たない 2 候補を宣言順に並べたルート
    // When: 解決する
    // Then: 属性未確定グループ内の宣言順先頭候補が選ばれる
    #[test]
    fn resolve_falls_back_to_unconfirmed_candidate() {
        let profiles = vec![
            profile("first", "model-first"),
            profile("second", "model-second"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("first", None), candidate("second", None)],
        )]);
        let catalog = build_catalog(&[], &["model-first", "model-second"]);
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("属性未確定でも利用可能なら解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "first".to_string(),
                model_id: "model-first".to_string(),
            }
        );
    }

    // Given: 何もピンしていないセッション
    // When: 解決する
    // Then: 勝者となったプロファイルがセッションへピンされる
    #[test]
    fn resolve_pins_winner_into_affinity() {
        let profiles = vec![profile("primary", "model-a")];
        let routing = routing_config(&[("summary", vec![candidate("primary", None)])]);
        let catalog = build_catalog(&[("model-a", Availability::Available)], &[]);
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("利用可能な候補から解決できる");

        assert_eq!(resolved.profile, "primary");
        assert_eq!(
            affinity
                .pinned("session-1", "summary")
                .map(|pin| (pin.profile.as_str(), pin.model.as_str())),
            Some(("primary", "model-a")),
            "勝者がピンされる"
        );
    }

    // Given: ルートテーブルに存在しない論理モデル
    // When: 解決する
    // Then: UnknownLogicalModel を返す
    #[test]
    fn resolve_unknown_logical_model_errors() {
        let profiles = vec![profile("primary", "model-a")];
        let routing = routing_config(&[("summary", vec![candidate("primary", None)])]);
        let catalog = build_catalog(&[("model-a", Availability::Available)], &[]);
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();

        let error = router
            .resolve(&mut affinity, "session-1", &logical("missing"))
            .expect_err("未知の論理モデルは解決できない");

        assert_eq!(
            error,
            RoutingError::UnknownLogicalModel("missing".to_string())
        );
    }

    // Given: すべての候補が利用不可 (カタログ上 Unavailable、または不在) なルート
    // When: 解決する
    // Then: NoAvailableCandidate を論理モデル名付きで返す
    #[test]
    fn resolve_no_available_candidate_errors() {
        let profiles = vec![
            profile("unavailable", "model-unavailable"),
            profile("absent", "model-absent"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("unavailable", None), candidate("absent", None)],
        )]);
        let catalog = build_catalog(&[("model-unavailable", Availability::Unavailable)], &[]);
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();

        let error = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect_err("利用可能な候補がないなら解決できない");

        assert_eq!(
            error,
            RoutingError::NoAvailableCandidate("summary".to_string())
        );
    }

    // Given: default_model と異なる実モデル ID を上書き指定した候補
    // When: 解決する
    // Then: 上書き指定の実モデル ID で解決される
    #[test]
    fn resolve_candidate_model_override_uses_override_id() {
        let profiles = vec![profile("primary", "model-default")];
        let routing = routing_config(&[(
            "summary",
            vec![candidate("primary", Some("model-override"))],
        )]);
        let catalog = build_catalog(
            &[
                ("model-default", Availability::Available),
                ("model-override", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("上書きモデルが利用可能なら解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "primary".to_string(),
                model_id: "model-override".to_string(),
            }
        );
    }

    // Given: 存在しないプロファイルを参照するルート候補を含む構成
    // When: Router を構築する
    // Then: UnknownProfile を参照名付きで返す
    #[test]
    fn router_new_rejects_candidate_with_unknown_profile() {
        let profiles = vec![profile("primary", "model-a")];
        let routing = routing_config(&[("summary", vec![candidate("ghost", None)])]);
        let catalog = build_catalog(&[("model-a", Availability::Available)], &[]);

        let error = Router::new(profiles, &routing, catalog)
            .expect_err("未知のプロファイル参照は構築を拒否する");

        assert_eq!(error, RoutingError::UnknownProfile("ghost".to_string()));
    }

    // Given: 同名のプロファイルが 2 つ含まれる構成
    // When: Router を構築する
    // Then: 重複した名前を理由に含む InvalidProfile を返す
    #[test]
    fn router_new_rejects_duplicate_profile_names() {
        let profiles = vec![profile("primary", "model-a"), profile("primary", "model-b")];
        let routing = routing_config(&[]);
        let catalog = build_catalog(&[], &[]);

        let error = Router::new(profiles, &routing, catalog)
            .expect_err("重複するプロファイル名は構築を拒否する");

        assert_eq!(
            error,
            RoutingError::InvalidProfile {
                reason: "プロファイル名 'primary' が重複しています".to_string()
            }
        );
    }

    fn cooling_router(health: &std::sync::Arc<crate::ProviderHealth>) -> Router {
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("primary", None),
                candidate("secondary", None),
                candidate("tertiary", None),
            ],
        )]);
        let catalog = build_catalog(
            &[
                ("model-a", Availability::Available),
                ("model-b", Availability::Available),
                ("model-c", Availability::Available),
            ],
            &[],
        );
        Router::new(
            vec![
                profile("primary", "model-a"),
                profile("secondary", "model-b"),
                profile("tertiary", "model-c"),
            ],
            &routing,
            catalog,
        )
        .expect("有効な構成で Router を構築できる")
        .with_health(std::sync::Arc::clone(health))
    }

    // Given: 先頭候補へピン済みのセッションと、先頭プロファイルの quota クールダウン
    // When: 解決する
    // Then: ピンとクールダウン中の候補を飛ばし、次の候補へピンし直す
    #[test]
    fn resolve_skips_cooling_pin_and_candidates() {
        let health = std::sync::Arc::new(crate::ProviderHealth::default());
        let router = cooling_router(&health);
        let mut affinity = SessionAffinity::default();
        affinity.pin(
            "session-1",
            "summary",
            RoutePin {
                position: 0,
                profile: "primary".into(),
                model: "model-a".into(),
            },
        );
        health.record_failure(
            "primary",
            "model-a",
            FailureKind::Quota,
            None,
            std::time::Instant::now(),
        );

        let resolved = router
            .resolve(&mut affinity, "session-1", &logical("summary"))
            .expect("クールダウン外の候補へ解決できる");

        assert_eq!(resolved, failed_route("secondary", "model-b"));
        assert_eq!(
            affinity
                .pinned("session-1", "summary")
                .map(|pin| (pin.profile.as_str(), pin.model.as_str())),
            Some(("secondary", "model-b"))
        );
    }

    // Given: 全候補のプロファイルが認証エラーでクールダウン中 / When: 解決する
    // Then: 候補なしのエラーになる
    #[test]
    fn resolve_fails_when_every_candidate_is_cooling() {
        let health = std::sync::Arc::new(crate::ProviderHealth::default());
        let router = cooling_router(&health);
        let now = std::time::Instant::now();
        for (profile, model) in [
            ("primary", "model-a"),
            ("secondary", "model-b"),
            ("tertiary", "model-c"),
        ] {
            health.record_failure(profile, model, FailureKind::Auth, None, now);
        }

        let result = router.resolve(
            &mut SessionAffinity::default(),
            "session-1",
            &logical("summary"),
        );

        assert_eq!(
            result,
            Err(RoutingError::NoAvailableCandidate("summary".into()))
        );
    }
}
