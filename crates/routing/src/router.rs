//! 論理モデルをプロバイダプロファイルと実モデル ID へ解決するルーターです。

use std::collections::BTreeMap;

use crate::{FailureKind, ProviderProfile, RoutingError, SessionAffinity};
use model::{LogicalModelId, ModelCatalog};

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
/// 論理モデルを具体ルートへ解決します。
///
/// ルートテーブルは [`BTreeMap`] で保持するため、論理モデルをまたぐ反復順序は
/// 辞書順になります。ただし本実装の解決は 1 つの論理モデルを独立に扱い、
/// 論理モデル間の順序付け (一括再解決など) は扱いません。
#[derive(Debug, Clone, PartialEq)]
pub struct Router {
    /// プロファイル名をキーとした検証済みプロバイダプロファイル。
    profiles: BTreeMap<String, ProviderProfile>,
    /// 論理モデル名から候補リスト (宣言順) へのルートテーブル。
    routes: BTreeMap<String, Vec<config::RouteCandidateConfig>>,
    /// 利用可否の判定に使用するモデルカタログ。
    catalog: ModelCatalog,
}

impl Router {
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
        })
    }

    /// セッションのアフィニティを考慮して論理モデルを具体ルートへ解決します。
    ///
    /// 解決手順:
    /// 1. セッションが論理モデルをピンしていて、ピン先プロファイルが存在し、
    ///    その concrete model (= ピン先プロファイルの `default_model`) がカタログ上
    ///    利用可能なら、そのルートを再ピンせずに返します。
    ///    ピンはプロファイル名のみを保持するため、モデル上書き付き候補でピンされた
    ///    場合でも、再解決時の concrete model は `default_model` に寄せられます。
    /// 2. 以外の場合、ルートテーブルの候補を評価します。候補の concrete model は
    ///    `model` 上書き (指定時) またはプロファイルの `default_model` で、
    ///    カタログ上利用可能 (存在かつ Available) な候補のみが選択されます。
    ///    カタログ項目が属性未確定 (`attributes_confirmed == false`) の候補は
    ///    確定済み候補の後に回され、各グループ内では宣言順を維持します。
    /// 3. 選ばれた候補は返却前にアフィニティへピンされます。
    ///
    /// # Errors
    /// - 論理モデルがルートテーブルに存在しない場合、
    ///   [`RoutingError::UnknownLogicalModel`] を返します。
    /// - 利用可能な候補が 1 つもない場合、[`RoutingError::NoAvailableCandidate`] を返します。
    pub fn resolve(
        &self,
        affinity: &mut SessionAffinity,
        session_id: &str,
        logical: &LogicalModelId,
    ) -> Result<ResolvedRoute, RoutingError> {
        let logical_name = logical.as_str();

        if let Some(pinned_name) = affinity.pinned(session_id, logical_name)
            && let Some(profile) = self.profiles.get(pinned_name)
            && self.catalog.is_available(&profile.default_model)
        {
            return Ok(ResolvedRoute {
                profile: pinned_name.to_string(),
                model_id: profile.default_model.clone(),
            });
        }

        let candidates = self
            .routes
            .get(logical_name)
            .ok_or_else(|| RoutingError::UnknownLogicalModel(logical_name.to_string()))?;

        let mut confirmed: Vec<(&str, &str)> = Vec::new();
        let mut unconfirmed: Vec<(&str, &str)> = Vec::new();
        for candidate in candidates {
            let Some(profile) = self.profiles.get(&candidate.profile) else {
                continue; // new() で検証済みのため通常は到達しない
            };
            let model_id = candidate.model.as_deref().unwrap_or(&profile.default_model);
            if !self.catalog.is_available(model_id) {
                continue;
            }
            let attributes_confirmed = self
                .catalog
                .get(model_id)
                .is_some_and(|entry| entry.attributes_confirmed);
            let group = if attributes_confirmed {
                &mut confirmed
            } else {
                &mut unconfirmed
            };
            group.push((profile.name.as_str(), model_id));
        }

        let (profile_name, model_id) = confirmed
            .into_iter()
            .chain(unconfirmed)
            .next()
            .ok_or_else(|| RoutingError::NoAvailableCandidate(logical_name.to_string()))?;

        affinity.pin(session_id, logical_name, profile_name);
        Ok(ResolvedRoute {
            profile: profile_name.to_string(),
            model_id: model_id.to_string(),
        })
    }

    /// 障害の発生したプロファイルの次のフォールバック先を解決します。
    ///
    /// ADR 0004 のフォールバック順 (失敗プロファイル → 同一論理モデルの別候補 →
    /// 別の論理モデル) に従い、次の順で候補を走査します。
    ///
    /// 1. 同一論理モデル: 失敗プロファイルと一致する先頭候補の位置より厳密に後続する
    ///    候補を宣言順に走査し、最初に利用可能な候補を返します。
    ///    `failed_profile` が候補に存在しない場合は失敗位置を先頭より前とみなし、
    ///    全候補を走査対象にします。
    /// 2. 別の論理モデル: (1) で候補が見つからない場合、ルートテーブル上の他の全
    ///    論理モデルを走査します。各論理モデルでは宣言順に最初の利用可能候補を
    ///    採ります。v0.1 のルートテーブルは [`BTreeMap`] のため、論理モデル間の
    ///    走査順は宣言順ではなく辞書順になる点に注意してください。
    /// 3. いずれでも利用可能候補が見つからない場合は `None` を返します。
    ///
    /// 利用可否の判定は [`Router::resolve`] と同じく、concrete model
    /// (= `model` 上書き指定時はその値、それ以外はプロファイルの `default_model`)
    /// がカタログ上で存在かつ Available であることです。
    /// [`Router::resolve`] と異なり、`attributes_confirmed` による属性未確定候補の
    /// 後回しは行いません。フォールバック時は属性の確定度よりも可用性の復旧を
    /// 優先するためです。
    ///
    /// 勝者を見つけた場合は `session_id` と `logical` を勝者プロファイルへ再ピンして
    /// [`ResolvedRoute`] を返します。別の論理モデルの候補で勝った場合も、
    /// 再ピン先はあくまで元の `logical` に対してです。
    /// 見つからなかった場合はアフィニティを変更せず `None` を返します。
    ///
    /// `failure` は障害種別の観測と将来の順序付け改善のために受け取りますが、
    /// v0.1 ではフォールバック順序には影響しません。本 crate はログ出力を行わない
    /// ため、値自体も v0.1 では使用しません。
    pub fn next_fallback(
        &self,
        affinity: &mut SessionAffinity,
        session_id: &str,
        logical: &LogicalModelId,
        failed_profile: &str,
        failure: FailureKind,
    ) -> Option<ResolvedRoute> {
        // v0.1 では failure は順序付けに使用しない (観測・将来利用のための引数)。
        let _ = failure;
        let logical_name = logical.as_str();

        if let Some(candidates) = self.routes.get(logical_name) {
            let remaining_after_failed = candidates
                .iter()
                .position(|candidate| candidate.profile == failed_profile)
                .map_or(candidates.as_slice(), |index| &candidates[index + 1..]);
            for candidate in remaining_after_failed {
                if let Some(route) = self.available_route(candidate) {
                    affinity.pin(session_id, logical_name, &route.profile);
                    return Some(route);
                }
            }
        }

        for (other_logical, candidates) in &self.routes {
            if other_logical == logical_name {
                continue;
            }
            for candidate in candidates {
                if let Some(route) = self.available_route(candidate) {
                    affinity.pin(session_id, logical_name, &route.profile);
                    return Some(route);
                }
            }
        }

        None
    }

    /// 候補の concrete model がカタログ上利用可能なら [`ResolvedRoute`] を返します。
    ///
    /// 利用可否の判定のみを行い、`attributes_confirmed` による優先順位付けは
    /// 行いません (フォールバックでは可用性のみを見るため)。
    fn available_route(&self, candidate: &config::RouteCandidateConfig) -> Option<ResolvedRoute> {
        // new() で検証済みのため、未知のプロファイルは通常発生しない。
        let profile = self.profiles.get(&candidate.profile)?;
        let model_id = candidate.model.as_deref().unwrap_or(&profile.default_model);
        if !self.catalog.is_available(model_id) {
            return None;
        }
        Some(ResolvedRoute {
            profile: profile.name.clone(),
            model_id: model_id.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{ResolvedRoute, Router};
    use crate::profile::ProviderProfile;
    use crate::{FailureKind, RoutingError, SessionAffinity};
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
            models: vec![default_model.to_string()],
            default_model: default_model.to_string(),
        };
        ProviderProfile::try_from((name, &profile_config)).expect("有効な設定は変換できる")
    }

    fn candidate(profile: &str, model: Option<&str>) -> config::RouteCandidateConfig {
        config::RouteCandidateConfig {
            profile: profile.to_string(),
            model: model.map(str::to_string),
        }
    }

    fn routing_config(
        routes: &[(&str, Vec<config::RouteCandidateConfig>)],
    ) -> config::RoutingConfig {
        config::RoutingConfig {
            routes: routes
                .iter()
                .map(|(logical, candidates)| (logical.to_string(), candidates.clone()))
                .collect(),
        }
    }

    fn logical(name: &str) -> LogicalModelId {
        LogicalModelId::from(name)
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
            source: CatalogSource::Builtin,
            attributes_confirmed: true,
        }
    }

    fn build_catalog(confirmed: &[(&str, Availability)], discovered: &[&str]) -> ModelCatalog {
        let mut catalog = ModelCatalog::builtin();
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

    // Given: ピン済みセッションと、ピン先とは別の候補を宣言したルート
    // When: ピン先の default_model がカタログ上利用可能なまま解決する
    // Then: ピン先のルートを再ピンなしで使い、ピンは変化しない
    #[test]
    fn resolve_uses_pinned_profile_when_available() {
        let profiles = vec![
            profile("primary", "model-a"),
            profile("secondary", "model-b"),
        ];
        let routing = routing_config(&[("summary", vec![candidate("secondary", None)])]);
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
        affinity.pin("session-1", "summary", "primary");

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
            affinity.pinned("session-1", "summary"),
            Some("primary"),
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
        affinity.pin("session-1", "summary", "primary");

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
    // Then: 宣言順後位の属性確定済み候補が選ばれる
    #[test]
    fn resolve_deprioritizes_unconfirmed_catalog_entries() {
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
            .expect("属性確定済み候補から解決できる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "confirmed-second".to_string(),
                model_id: "model-confirmed".to_string(),
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
            affinity.pinned("session-1", "summary"),
            Some("primary"),
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

    // Given: 3 候補 [a, b, c] を宣言順に持つ論理モデルのルート
    // When: 失敗プロファイルとして a を指定してフォールバックする / b を指定してフォールバックする
    // Then: a の失敗時は b、b の失敗時は c が次の候補として返る
    #[test]
    fn fallback_same_logical_next_profile_in_order() {
        let profiles = vec![
            profile("a", "model-a"),
            profile("b", "model-b"),
            profile("c", "model-c"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("a", None),
                candidate("b", None),
                candidate("c", None),
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
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "a",
                FailureKind::Server,
            )
            .expect("失敗プロファイルより後続の候補へフォールバックできる");
        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "b".to_string(),
                model_id: "model-b".to_string(),
            }
        );

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "b",
                FailureKind::Server,
            )
            .expect("失敗プロファイルより後続の候補へフォールバックできる");
        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "c".to_string(),
                model_id: "model-c".to_string(),
            }
        );
    }

    // Given: 失敗プロファイルの直後候補の concrete model がカタログ上利用不可な 3 候補
    // When: 失敗プロファイルを指定してフォールバックする
    // Then: 利用不可候補を飛ばし、さらに次の利用可能候補が選ばれる
    #[test]
    fn fallback_skips_unavailable_candidates() {
        let profiles = vec![
            profile("a", "model-a"),
            profile("b", "model-b"),
            profile("c", "model-c"),
        ];
        let routing = routing_config(&[(
            "summary",
            vec![
                candidate("a", None),
                candidate("b", None),
                candidate("c", None),
            ],
        )]);
        let catalog = build_catalog(
            &[
                ("model-a", Availability::Available),
                ("model-b", Availability::Unavailable),
                ("model-c", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "a",
                FailureKind::Server,
            )
            .expect("利用可能な後続候補が 1 つ先にある");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "c".to_string(),
                model_id: "model-c".to_string(),
            }
        );
    }

    // Given: 同一論理モデルの候補を使い切る構成。他論理モデルとして
    //        "aaa-other" と "zzz-other" を持つ (設定上は zzz を先に宣言)
    // When: 同一論理モデルの失敗プロファイルでフォールバックする
    // Then: ルートテーブルは BTreeMap のため辞書順で走査され、
    //       "aaa-other" の最初の利用可能候補が (宣言順ではなく) 選ばれる
    #[test]
    fn fallback_crosses_to_next_logical_model_lexicographic() {
        let profiles = vec![
            profile("first", "model-first"),
            profile("aaa-profile", "model-aaa"),
            profile("zzz-profile", "model-zzz"),
        ];
        let routing = routing_config(&[
            ("summary", vec![candidate("first", None)]),
            ("zzz-other", vec![candidate("zzz-profile", None)]),
            ("aaa-other", vec![candidate("aaa-profile", None)]),
        ]);
        let catalog = build_catalog(
            &[
                ("model-first", Availability::Available),
                ("model-aaa", Availability::Available),
                ("model-zzz", Availability::Available),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");

        let mut affinity = SessionAffinity::default();
        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "first",
                FailureKind::Server,
            )
            .expect("別の論理モデルの候補へフォールバックできる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "aaa-profile".to_string(),
                model_id: "model-aaa".to_string(),
            }
        );
    }

    // Given: 同一論理モデルに後続候補がなく、他の論理モデルの候補も利用不可な構成
    // When: 失敗プロファイルを指定してフォールバックする
    // Then: None を返し、アフィニティのピンは変化しない
    #[test]
    fn fallback_exhausted_returns_none() {
        let profiles = vec![
            profile("only", "model-only"),
            profile("other", "model-other"),
        ];
        let routing = routing_config(&[
            ("summary", vec![candidate("only", None)]),
            ("other-logical", vec![candidate("other", None)]),
        ]);
        let catalog = build_catalog(
            &[
                ("model-only", Availability::Available),
                ("model-other", Availability::Unavailable),
            ],
            &[],
        );
        let router =
            Router::new(profiles, &routing, catalog).expect("有効な構成で Router を構築できる");
        let mut affinity = SessionAffinity::default();
        affinity.pin("session-1", "summary", "only");

        let resolved = router.next_fallback(
            &mut affinity,
            "session-1",
            &logical("summary"),
            "only",
            FailureKind::Server,
        );

        assert!(resolved.is_none(), "利用可能候補がどこにもないなら None");
        assert_eq!(
            affinity.pinned("session-1", "summary"),
            Some("only"),
            "使い切り時はピンが変化しない"
        );
    }

    // Given: 2 候補を持つルートと、先頭プロファイルへピン済みのセッション
    // When: ピン先プロファイルを失敗プロファイルとしてフォールバックする
    // Then: 勝者となった次候補がセッションへ再ピンされる
    #[test]
    fn fallback_updates_affinity_pin() {
        let profiles = vec![profile("a", "model-a"), profile("b", "model-b")];
        let routing =
            routing_config(&[("summary", vec![candidate("a", None), candidate("b", None)])]);
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
        affinity.pin("session-1", "summary", "a");

        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "a",
                FailureKind::Server,
            )
            .expect("次候補へフォールバックできる");

        assert_eq!(resolved.profile, "b");
        assert_eq!(
            affinity.pinned("session-1", "summary"),
            Some("b"),
            "勝者が再ピンされる"
        );
    }

    // Given: 失敗プロファイル名が論理モデルの候補に存在しないルート
    // When: その名前を失敗プロファイルとしてフォールバックする
    // Then: 失敗位置を先頭より前とみなし、最初の利用可能候補が選ばれる
    #[test]
    fn fallback_failed_profile_not_in_route_is_ignored() {
        let profiles = vec![profile("a", "model-a"), profile("b", "model-b")];
        let routing =
            routing_config(&[("summary", vec![candidate("a", None), candidate("b", None)])]);
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
        let resolved = router
            .next_fallback(
                &mut affinity,
                "session-1",
                &logical("summary"),
                "ghost",
                FailureKind::Server,
            )
            .expect("全候補が走査対象になる");

        assert_eq!(
            resolved,
            ResolvedRoute {
                profile: "a".to_string(),
                model_id: "model-a".to_string(),
            }
        );
    }
}
