//! ルーティングに関する設定型を定義します。

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::provider::{ModelKind, ProviderProfileConfig};

/// ルーティング設定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RoutingConfig {
    /// ルート名から候補リスト (フォールバック順) へのマップ。
    pub routes: BTreeMap<String, Vec<RouteCandidateConfig>>,
    /// 失敗したプロファイル・モデルを候補から外す期間。
    #[serde(skip_serializing_if = "CooldownConfig::is_default")]
    pub cooldown: CooldownConfig,
    /// ロールのチェーンをフォールバックするときの待機設定。
    #[serde(skip_serializing_if = "FallbackConfig::is_default")]
    pub fallback: FallbackConfig,
}

/// チェーンのフォールバック設定 (ADR 0030)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct FallbackConfig {
    /// 一時的な失敗でプールが尽きたとき、次のプールへ進む前に Retry-After を待つ上限秒数。
    /// これより長い Retry-After は待たずに次のプールへ進む。0 なら待たない。
    pub max_retry_after_wait_secs: u64,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self {
            max_retry_after_wait_secs: 30,
        }
    }
}

impl FallbackConfig {
    /// 既定値と等しいか (保存時に省略できるか)。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// 失敗後のクールダウン期間 (ADR 0030)。
///
/// Retry-After などプロバイダが期間を示した場合はそちらを優先する。認証エラーは
/// 期間を持たず、runtime の再構成 (設定の再読み込み) まで続く。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CooldownConfig {
    /// 支払い・利用上限による失敗でプロファイル全体を止める秒数。
    pub quota_secs: u64,
    /// レート制限による失敗で (プロファイル, モデル) を止める秒数。
    pub rate_limit_secs: u64,
}

impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            quota_secs: 1_800,
            rate_limit_secs: 30,
        }
    }
}

impl CooldownConfig {
    /// 既定値と等しいか (保存時に省略できるか)。
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// ルート (同じモデルへの経路プール) の候補 1 件分の設定。
///
/// 推論強度はロールのチェーンの要素 ([`crate::ChainEntryConfig`]) で指定する。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct RouteCandidateConfig {
    /// 使用するプロバイダプロファイル名。
    pub profile: String,
    /// モデル ID の上書き指定。省略時はプロファイルの `default_model` を使用する。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 同じモデルの自動グルーピングで追加された候補か。
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
}

impl RouteCandidateConfig {
    /// 候補が送るモデル ID。`model` の上書き、なければ profile の `default_model`。
    /// profile が未定義で `model` もなければ `None`。
    pub fn model_id<'a>(
        &'a self,
        providers: &'a BTreeMap<String, ProviderProfileConfig>,
    ) -> Option<&'a str> {
        self.model.as_deref().or_else(|| {
            providers
                .get(&self.profile)
                .map(|profile| profile.default_model.as_str())
        })
    }

    /// 候補の実効の種別 (ADR 0038)。
    ///
    /// `model` を明示した場合はその profile のモデル登録 (なければ ID からの推定)、
    /// 省略した場合は profile の `default_model` の種別。profile が未定義で `model` もなければ
    /// 決められないので `None`。
    pub fn kind(&self, providers: &BTreeMap<String, ProviderProfileConfig>) -> Option<ModelKind> {
        let model = self.model_id(providers)?;
        Some(providers.get(&self.profile).map_or_else(
            || ModelKind::infer(model),
            |profile| profile.model_kind(model),
        ))
    }
}

/// route の候補の種別のうち、決められたもの。
///
/// 空なら種別の決まらない route、2 つなら種別の混ざった route (ADR 0038 で禁止)。
pub fn route_model_kinds(
    candidates: &[RouteCandidateConfig],
    providers: &BTreeMap<String, ProviderProfileConfig>,
) -> BTreeSet<ModelKind> {
    candidates
        .iter()
        .filter_map(|candidate| candidate.kind(providers))
        .collect()
}

/// チェーンの用途。用途ごとに route の候補が満たすべき条件が決まる (ADR 0038)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainPurpose {
    /// 通常のロール・カテゴリ・compactor のチェーン。候補はすべて会話用のモデル。
    Conversation,
    /// `agents.roles.image_generator` のチェーン。候補はすべて画像生成用のモデルで、
    /// 画像生成に対応した provider (`openai-codex` / `openai`) の profile。
    ImageGeneration,
}

impl ChainPurpose {
    /// [`crate::types::agents::explicit_refs`] のアドレスが指すチェーンの用途。
    pub fn of_address(address: &str) -> Self {
        if address == crate::types::agents::IMAGE_GENERATOR_ADDRESS {
            Self::ImageGeneration
        } else {
            Self::Conversation
        }
    }

    /// 候補に求める種別。
    pub const fn model_kind(self) -> ModelKind {
        match self {
            Self::Conversation => ModelKind::Chat,
            Self::ImageGeneration => ModelKind::ImageGeneration,
        }
    }

    /// この用途のチェーンが候補に求めること (エラーの説明用)。
    pub const fn requirement(self) -> &'static str {
        match self {
            Self::Conversation => {
                "conversation and compactor chains accept only chat models (ADR 0038)"
            }
            Self::ImageGeneration => {
                "the image_generator chain accepts only image_generation models of openai-codex or openai profiles (ADR 0038)"
            }
        }
    }

    /// 候補がこの用途に合わない理由。合う場合と、profile も `model` もなく種別を
    /// 決められない場合 (未定義の profile は別の検査が扱う) は `None`。
    pub fn candidate_problem(
        self,
        candidate: &RouteCandidateConfig,
        providers: &BTreeMap<String, ProviderProfileConfig>,
    ) -> Option<String> {
        let kind = candidate.kind(providers)?;
        let model = candidate.model_id(providers).unwrap_or_default();
        if kind != self.model_kind() {
            return Some(format!(
                "candidate `{}/{model}` is a {kind} model",
                candidate.profile
            ));
        }
        let provider_type = providers.get(&candidate.profile)?.provider_type;
        (self == Self::ImageGeneration && !provider_type.supports_image_generation()).then(|| {
            format!(
                "candidate `{}/{model}` uses a {} profile, which cannot generate images",
                candidate.profile,
                provider_type.as_str()
            )
        })
    }

    /// route のすべての候補がこの用途に合うか。候補のない route はどの用途にも合う。
    pub fn accepts_route(
        self,
        candidates: &[RouteCandidateConfig],
        providers: &BTreeMap<String, ProviderProfileConfig>,
    ) -> bool {
        candidates
            .iter()
            .all(|candidate| self.candidate_problem(candidate, providers).is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn providers() -> BTreeMap<String, ProviderProfileConfig> {
        let profile: ProviderProfileConfig = toml::from_str(
            r#"
provider_type = "openai"
models = ["gpt-5", { id = "studio", enabled = true, kind = "image_generation" }, { id = "gpt-image-legacy", enabled = true, kind = "chat" }]
default_model = "gpt-5"
"#,
        )
        .expect("profile");
        let painter: ProviderProfileConfig = toml::from_str(
            "provider_type = 'openai-compatible'\nmodels = ['gpt-image-1']\ndefault_model = 'gpt-image-1'",
        )
        .expect("profile");
        BTreeMap::from([("openai".into(), profile), ("painter".into(), painter)])
    }

    fn candidate(profile: &str, model: Option<&str>) -> RouteCandidateConfig {
        RouteCandidateConfig {
            profile: profile.into(),
            model: model.map(Into::into),
            auto: false,
        }
    }

    // Given: model の明示 (登録あり・登録なし)・省略・未定義の profile の候補
    // When: 候補の実効の種別を求める
    // Then: 登録、名前の推定、default_model の順に決まり、決められなければ None
    #[test]
    fn candidate_kind_follows_registration_inference_or_default_model() {
        let providers = providers();
        for (candidate, kind) in [
            (
                candidate("openai", Some("studio")),
                Some(ModelKind::ImageGeneration),
            ),
            (
                candidate("openai", Some("gpt-image-legacy")),
                Some(ModelKind::Chat),
            ),
            (
                candidate("openai", Some("gpt-image-1")),
                Some(ModelKind::ImageGeneration),
            ),
            (candidate("openai", None), Some(ModelKind::Chat)),
            (candidate("painter", None), Some(ModelKind::ImageGeneration)),
            (
                candidate("missing", Some("dall-e-3")),
                Some(ModelKind::ImageGeneration),
            ),
            (candidate("missing", None), None),
        ] {
            assert_eq!(candidate.kind(&providers), kind, "{candidate:?}");
        }
        assert_eq!(
            route_model_kinds(
                &[
                    candidate("openai", None),
                    candidate("openai", Some("studio"))
                ],
                &providers
            ),
            BTreeSet::from([ModelKind::Chat, ModelKind::ImageGeneration])
        );
        assert!(route_model_kinds(&[candidate("missing", None)], &providers).is_empty());
    }

    // Given: 会話用・画像生成用・対応しない provider の画像モデルの候補
    // When: チェーンの用途ごとに候補を検査する
    // Then: 用途に合わない候補だけが理由付きで拒まれ、候補のない route はどちらにも合う
    #[test]
    fn chain_purposes_accept_only_their_kind_and_providers() {
        let providers = providers();
        let chat = candidate("openai", None);
        let image = candidate("openai", Some("studio"));
        let unsupported = candidate("painter", None);

        assert_eq!(
            ChainPurpose::Conversation.candidate_problem(&chat, &providers),
            None
        );
        assert!(
            ChainPurpose::Conversation
                .candidate_problem(&image, &providers)
                .is_some_and(|problem| problem.contains("openai/studio")
                    && problem.contains("image_generation"))
        );
        assert_eq!(
            ChainPurpose::ImageGeneration.candidate_problem(&image, &providers),
            None
        );
        assert!(
            ChainPurpose::ImageGeneration
                .candidate_problem(&chat, &providers)
                .is_some_and(|problem| problem.contains("openai/gpt-5") && problem.contains("chat"))
        );
        assert!(
            ChainPurpose::ImageGeneration
                .candidate_problem(&unsupported, &providers)
                .is_some_and(|problem| problem.contains("openai-compatible"))
        );
        for purpose in [ChainPurpose::Conversation, ChainPurpose::ImageGeneration] {
            assert!(purpose.accepts_route(&[], &providers));
            assert!(!purpose.accepts_route(&[chat.clone(), image.clone()], &providers));
        }
        assert_eq!(
            ChainPurpose::of_address("roles.image_generator"),
            ChainPurpose::ImageGeneration
        );
        assert_eq!(
            ChainPurpose::of_address("roles.compactor"),
            ChainPurpose::Conversation
        );
    }

    // Given: model キーあり・なしの候補 TOML / When: それぞれ解析する
    // Then: 省略時は None、指定時は Some になる
    #[test]
    fn route_candidate_optional_model_deserializes() {
        let without_model: RouteCandidateConfig =
            toml::from_str(r#"profile = "anthropic-main""#).expect("model 省略の候補を解析できる");
        assert_eq!(without_model.profile, "anthropic-main");
        assert_eq!(without_model.model, None);

        let with_model: RouteCandidateConfig = toml::from_str(
            r#"
profile = "anthropic-main"
model = "claude-opus-4-1"
"#,
        )
        .expect("model 指定の候補を解析できる");
        assert_eq!(with_model.profile, "anthropic-main");
        assert_eq!(with_model.model.as_deref(), Some("claude-opus-4-1"));
        assert!(!with_model.auto);

        let auto: RouteCandidateConfig = toml::from_str(
            r#"
profile = "openai-main"
model = "gpt-5"
auto = true
"#,
        )
        .expect("auto 指定の候補を解析できる");
        assert!(auto.auto);
        assert!(
            toml::from_str::<RouteCandidateConfig>(
                "profile = \"p\"\nreasoning_effort = \"high\"\n"
            )
            .is_err(),
            "推論強度は候補に置けない"
        );
    }
}
