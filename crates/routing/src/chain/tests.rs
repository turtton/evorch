use std::sync::Arc;
use std::time::{Duration, Instant};

use config::ChainEntryConfig;
use model::{Availability, CatalogCapabilities, CatalogEntry, CatalogSource, ModelCatalog};

use super::{Chain, FallbackCycle, FallbackStep};
use crate::{FailureKind, ProviderHealth, ProviderProfile, RoutePin, Router, SessionAffinity};

const WAIT: Duration = Duration::from_secs(30);

fn profile(name: &str) -> ProviderProfile {
    let config = config::ProviderProfileConfig {
        provider_type: config::ProviderTypeConfig::OpenAi,
        api_protocol: config::ApiProtocolConfig::OpenAiResponses,
        base_url: "https://api.example.test".into(),
        credential: config::CredentialRefConfig::Env {
            var: "API_KEY".into(),
        },
        models: vec![config::ModelEntryConfig::enabled(format!("{name}-model"))],
        excluded_models: Vec::new(),
        default_model: format!("{name}-model"),
    };
    ProviderProfile::try_from((name, &config)).expect("profile")
}

/// `routes` の各 route に、プロファイル名の候補を並べたルーター。全モデルが利用可能。
fn router(routes: &[(&str, &[&str])], health: Option<Arc<ProviderHealth>>) -> Router {
    let names: std::collections::BTreeSet<&str> = routes
        .iter()
        .flat_map(|(_, profiles)| profiles.iter().copied())
        .collect();
    let mut catalog = ModelCatalog::new();
    for name in &names {
        catalog.merge_models_dev(vec![CatalogEntry {
            model_id: format!("{name}-model"),
            provider: model::ProviderType::OpenAi,
            context_window: 64_000,
            max_output_tokens: 8_000,
            capabilities: CatalogCapabilities {
                tool_calling: true,
                reasoning: false,
                prompt_cache: false,
            },
            price: None,
            availability: Availability::Available,
            source: CatalogSource::ModelsDev,
            attributes_confirmed: true,
        }]);
    }
    let routing = config::RoutingConfig {
        routes: routes
            .iter()
            .map(|(route, profiles)| {
                (
                    (*route).to_owned(),
                    profiles
                        .iter()
                        .map(|profile| config::RouteCandidateConfig {
                            profile: (*profile).to_owned(),
                            ..Default::default()
                        })
                        .collect(),
                )
            })
            .collect(),
        ..Default::default()
    };
    let router =
        Router::new(names.into_iter().map(profile).collect(), &routing, catalog).expect("router");
    match health {
        Some(health) => router.with_health(health),
        None => router,
    }
}

fn chain(router: &Router, entries: &[ChainEntryConfig]) -> Chain {
    router.chain("planner", entries).expect("chain")
}

fn profiles_of(chain: &Chain, positions: &[usize]) -> Vec<String> {
    positions
        .iter()
        .map(|position| chain.member(*position).target.profile.clone())
        .collect()
}

/// 一周で送る順序を、全員が `transient` で失敗する前提で集める (待機は含めない)。
fn attempts(router: &Router, chain: &Chain, origin: usize, transient: bool) -> Vec<String> {
    let mut cycle = FallbackCycle::new(chain, origin);
    cycle.record_failure(origin, transient, None);
    let mut tried = Vec::new();
    loop {
        match cycle.next(router, chain, WAIT) {
            FallbackStep::Try(position) => {
                tried.push(position);
                cycle.record_failure(position, transient, None);
            }
            FallbackStep::Wait(_) => panic!("Retry-After がないので待たない"),
            FallbackStep::Exhausted => return profiles_of(chain, &tried),
        }
    }
}

fn kimi_then_claude() -> (Router, Chain) {
    let router = router(
        &[
            ("kimi-k3", &["kimi", "neuralwatt"]),
            ("claude", &["anthropic", "bedrock"]),
        ],
        None,
    );
    let chain = chain(
        &router,
        &[
            ChainEntryConfig::with_effort("kimi-k3", "high"),
            ChainEntryConfig::with_effort("claude", "medium"),
        ],
    );
    (router, chain)
}

// Given: 2 プールのチェーン / When: 展開する
// Then: 候補が宣言順に並び、要素の位置と推論強度を持つ。未知の route はエラー
#[test]
fn chain_expands_pools_in_order_with_entry_effort() {
    let (router, chain) = kimi_then_claude();

    let members: Vec<_> = chain
        .members()
        .iter()
        .map(|member| {
            (
                member.entry,
                member.target.profile.as_str(),
                member.reasoning_effort.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        members,
        [
            (0, "kimi", Some("high")),
            (0, "neuralwatt", Some("high")),
            (1, "anthropic", Some("medium")),
            (1, "bedrock", Some("medium")),
        ]
    );
    assert!(matches!(
        router.chain("planner", &[ChainEntryConfig::route("missing")]),
        Err(crate::RoutingError::UnknownLogicalModel(route)) if route == "missing"
    ));
}

// Given: 2 プール × 2 メンバーのチェーン
// When: 後ろのプールの先頭 (位置 2) が一時的な失敗をする
// Then: 同じプールの残り → 先頭のプール → 最後に元の位置、の順に一度ずつ試す
#[test]
fn transient_failure_scans_pool_first_then_wraps_and_retries_origin_last() {
    let (router, chain) = kimi_then_claude();

    assert_eq!(
        attempts(&router, &chain, 2, true),
        ["bedrock", "kimi", "neuralwatt", "anthropic"]
    );
}

// Given: 同じチェーン / When: 先頭のプールの 2 番目が一時的でない失敗をする
// Then: 同じプール内で前へ戻ってから次のプールへ進み、元の位置は再試行しない
#[test]
fn permanent_failure_wraps_within_pool_and_never_retries_origin() {
    let (router, chain) = kimi_then_claude();

    assert_eq!(
        attempts(&router, &chain, 1, false),
        ["kimi", "anthropic", "bedrock"]
    );
}

// Given: 同じプロファイル・モデルが 2 つのプールに現れるチェーン / When: 一周する
// Then: 各メンバーは一周につき一度だけ試す
#[test]
fn duplicate_member_is_tried_once_per_cycle() {
    let router = router(&[("a", &["shared", "x"]), ("b", &["shared", "y"])], None);
    let chain = chain(
        &router,
        &[ChainEntryConfig::route("a"), ChainEntryConfig::route("b")],
    );

    assert_eq!(attempts(&router, &chain, 1, false), ["shared", "y"]);
}

// Given: クールダウン中のメンバーを含むチェーン / When: 一周する
// Then: クールダウン中のメンバーには送らない
#[test]
fn cooling_members_are_skipped() {
    let health = Arc::new(ProviderHealth::default());
    health.record_failure(
        "neuralwatt",
        "neuralwatt-model",
        FailureKind::Quota,
        None,
        Instant::now(),
    );
    let router = router(
        &[
            ("kimi-k3", &["kimi", "neuralwatt"]),
            ("claude", &["anthropic"]),
        ],
        Some(health),
    );
    let chain = chain(
        &router,
        &[
            ChainEntryConfig::route("kimi-k3"),
            ChainEntryConfig::route("claude"),
        ],
    );

    assert_eq!(attempts(&router, &chain, 0, false), ["anthropic"]);
}

// Given: 1 メンバーのプールが短い Retry-After 付きで一時的に失敗した
// When: 次のプールへ進もうとする
// Then: 一度だけ待ち、同じメンバーを再試行し、二度目の失敗では待たずに次のプールへ進む
#[test]
fn short_retry_after_waits_once_before_leaving_pool() {
    let router = router(&[("kimi-k3", &["kimi"]), ("claude", &["anthropic"])], None);
    let chain = chain(
        &router,
        &[
            ChainEntryConfig::route("kimi-k3"),
            ChainEntryConfig::route("claude"),
        ],
    );
    let mut cycle = FallbackCycle::new(&chain, 0);
    cycle.record_failure(0, true, Some(Duration::from_secs(5)));

    assert_eq!(
        cycle.next(&router, &chain, WAIT),
        FallbackStep::Wait(Duration::from_secs(5))
    );
    assert_eq!(cycle.next(&router, &chain, WAIT), FallbackStep::Try(0));
    cycle.record_failure(0, true, Some(Duration::from_secs(5)));
    assert_eq!(cycle.next(&router, &chain, WAIT), FallbackStep::Try(1));
    cycle.record_failure(1, false, None);
    assert_eq!(cycle.next(&router, &chain, WAIT), FallbackStep::Exhausted);
}

// Given: Retry-After が上限を超える、または一時的でない失敗 / When: プールを離れる
// Then: 待たずに次のプールへ進む
#[test]
fn long_retry_after_or_permanent_failure_does_not_wait() {
    let router = router(&[("kimi-k3", &["kimi"]), ("claude", &["anthropic"])], None);
    let chain = chain(
        &router,
        &[
            ChainEntryConfig::route("kimi-k3"),
            ChainEntryConfig::route("claude"),
        ],
    );
    for (transient, retry_after) in [
        (true, Duration::from_secs(31)),
        (false, Duration::from_secs(1)),
    ] {
        let mut cycle = FallbackCycle::new(&chain, 0);
        cycle.record_failure(0, transient, Some(retry_after));

        assert_eq!(cycle.next(&router, &chain, WAIT), FallbackStep::Try(1));
    }
}

// Given: 新しいセッション / When: 解決する
// Then: 先頭の利用可能なメンバーを選び、位置ごとピンする
#[test]
fn new_session_resolves_from_head_and_pins_position() {
    let (router, chain) = kimi_then_claude();
    let mut affinity = SessionAffinity::default();

    assert_eq!(router.resolve_chain(&mut affinity, "run-1", &chain), Ok(0));
    assert_eq!(
        affinity.pinned("run-1", "planner"),
        Some(&RoutePin {
            position: 0,
            profile: "kimi".into(),
            model: "kimi-model".into(),
        })
    );
}

// Given: 後ろのプールにピンしたセッション / When: 解決する
// Then: 先頭が使えてもピンを維持する (元に戻さない)
#[test]
fn pin_is_kept_even_when_head_is_available() {
    let (router, chain) = kimi_then_claude();
    let mut affinity = SessionAffinity::default();
    crate::pin(&mut affinity, "run-1", &chain, 3);

    assert_eq!(router.resolve_chain(&mut affinity, "run-1", &chain), Ok(3));
}

// Given: ピン先がクールダウン中 / When: 解決する
// Then: 先頭ではなく、ピンの次からフォールバックと同じ順序で選び直してピンする
#[test]
fn unusable_pin_resolves_like_a_fallback_from_its_position() {
    let health = Arc::new(ProviderHealth::default());
    let router = router(
        &[
            ("kimi-k3", &["kimi", "neuralwatt"]),
            ("claude", &["anthropic", "bedrock"]),
        ],
        Some(Arc::clone(&health)),
    );
    let chain = chain(
        &router,
        &[
            ChainEntryConfig::route("kimi-k3"),
            ChainEntryConfig::route("claude"),
        ],
    );
    let mut affinity = SessionAffinity::default();
    crate::pin(&mut affinity, "run-1", &chain, 2);
    health.record_failure(
        "anthropic",
        "anthropic-model",
        FailureKind::Quota,
        None,
        Instant::now(),
    );

    assert_eq!(router.resolve_chain(&mut affinity, "run-1", &chain), Ok(3));
    assert_eq!(
        affinity.pinned("run-1", "planner").map(|pin| pin.position),
        Some(3)
    );
}

// Given: 設定から消えたメンバーへのピン (復元したピンなど) / When: 解決する
// Then: 元の位置にいたメンバーから選び直す
#[test]
fn pin_to_removed_member_resumes_from_its_former_position() {
    let (router, chain) = kimi_then_claude();
    let mut affinity = SessionAffinity::default();
    affinity.pin(
        "run-1",
        "planner",
        RoutePin {
            position: 2,
            profile: "removed".into(),
            model: "gone".into(),
        },
    );

    assert_eq!(router.resolve_chain(&mut affinity, "run-1", &chain), Ok(2));
}

// Given: 位置がずれたピン (設定でメンバーが増えた) / When: 解決する
// Then: 同じプロファイル・モデルのメンバーを引き直す
#[test]
fn shifted_pin_is_relocated_by_member_identity() {
    let (router, chain) = kimi_then_claude();
    let mut affinity = SessionAffinity::default();
    affinity.pin(
        "run-1",
        "planner",
        RoutePin {
            position: 0,
            profile: "bedrock".into(),
            model: "bedrock-model".into(),
        },
    );

    assert_eq!(router.resolve_chain(&mut affinity, "run-1", &chain), Ok(3));
}
