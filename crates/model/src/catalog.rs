//! オフラインで完結するモデルカタログの実装です。
//!
//! ADR 0013 のハイブリッド 4 供給源のうち、組み込みデフォルト・外部カタログ
//! (models.dev) のマージ・プロバイダ検出モデルのマージを担います。

use std::collections::BTreeMap;

use crate::types::{
    Availability, CatalogCapabilities, CatalogEntry, CatalogSource, ModelPrice, ProviderType,
};

/// [`ModelCatalog::supports`] で問い合わせる機能の種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    /// ツール呼び出し (function calling)。
    ToolCalling,
    /// 推論 (拡張思考)。
    Reasoning,
    /// プロンプトキャッシュ。
    PromptCache,
}

/// モデルカタログ。
///
/// モデル ID をキーにしたカタログ項目の集合です (ADR 0013)。
#[derive(Debug, Clone, PartialEq)]
pub struct ModelCatalog {
    /// モデル ID をキーとしたカタログ項目。
    entries: BTreeMap<String, CatalogEntry>,
}

impl ModelCatalog {
    /// カタログ項目のマップへの参照を返す。
    pub fn entries(&self) -> &BTreeMap<String, CatalogEntry> {
        &self.entries
    }

    /// 組み込みデフォルトのカタログを生成する。
    ///
    /// ADR 0013 の「組み込みデフォルト」供給源です。主要モデルの属性と
    /// 価格をオフラインで参照できる最小構成を保持します。
    pub fn builtin() -> Self {
        Self {
            entries: builtin_entries()
                .into_iter()
                .map(|entry| (entry.model_id.clone(), entry))
                .collect(),
        }
    }

    /// models.dev 等の外部カタログ取得結果をマージする。
    ///
    /// 同一モデル ID の組み込み項目は上書きします。マージされた項目は
    /// 供給源が `ModelsDev`・属性確定フラグが `true` に補正されます。
    pub fn merge_models_dev(&mut self, entries: Vec<CatalogEntry>) {
        for mut entry in entries {
            entry.source = CatalogSource::ModelsDev;
            entry.attributes_confirmed = true;
            self.entries.insert(entry.model_id.clone(), entry);
        }
    }

    /// プロバイダ API から検出したモデル ID をマージする。
    ///
    /// カタログに存在しない ID のみ挿入します。挿入された項目は属性未確定の
    /// プレースホルダ (`OpenAiCompatible`・サイズ 0・機能なし・価格なし) です。
    /// 既存項目 (属性確定済みか否かを問わず) は一切変更しません。
    pub fn merge_discovered(&mut self, model_ids: Vec<String>) {
        for model_id in model_ids {
            let placeholder = discovered_placeholder(&model_id);
            self.entries.entry(model_id).or_insert(placeholder);
        }
    }

    /// 指定 ID のカタログ項目を返す。
    ///
    /// 存在しない場合は `None` を返します。
    pub fn get(&self, model_id: &str) -> Option<&CatalogEntry> {
        self.entries.get(model_id)
    }

    /// 指定 ID のモデルが利用可能かどうかを返す。
    ///
    /// 存在しない場合や [`crate::types::Availability::Unavailable`] の
    /// 場合は `false` を返します。
    pub fn is_available(&self, model_id: &str) -> bool {
        self.get(model_id)
            .is_some_and(|entry| entry.availability == Availability::Available)
    }

    /// 指定 ID のモデルの価格情報を返す。
    ///
    /// 存在しない場合や価格不明の場合は `None` を返します。
    pub fn price_of(&self, model_id: &str) -> Option<&ModelPrice> {
        self.get(model_id).and_then(|entry| entry.price.as_ref())
    }

    /// 指定 ID のモデルが機能に対応しているかどうかを返す。
    ///
    /// 存在しない場合は `false` を返します。
    pub fn supports(&self, model_id: &str, capability: Capability) -> bool {
        self.get(model_id).is_some_and(|entry| {
            let capabilities = entry.capabilities;
            match capability {
                Capability::ToolCalling => capabilities.tool_calling,
                Capability::Reasoning => capabilities.reasoning,
                Capability::PromptCache => capabilities.prompt_cache,
            }
        })
    }
}

fn builtin_entries() -> Vec<CatalogEntry> {
    fn entry(
        model_id: &str,
        provider: ProviderType,
        context_window: u64,
        max_output_tokens: u64,
        capabilities: CatalogCapabilities,
        price: Option<ModelPrice>,
    ) -> CatalogEntry {
        CatalogEntry {
            model_id: model_id.to_string(),
            provider,
            context_window,
            max_output_tokens,
            capabilities,
            price,
            availability: Availability::Available,
            source: CatalogSource::Builtin,
            attributes_confirmed: true,
        }
    }

    vec![
        entry(
            "claude-sonnet-4-5",
            ProviderType::Anthropic,
            200_000,
            64_000,
            CatalogCapabilities {
                tool_calling: true,
                reasoning: true,
                prompt_cache: true,
            },
            Some(ModelPrice {
                input_per_million_usd: 3.0,
                output_per_million_usd: 15.0,
            }),
        ),
        entry(
            "claude-haiku-4-5",
            ProviderType::Anthropic,
            200_000,
            64_000,
            CatalogCapabilities {
                tool_calling: true,
                reasoning: false,
                prompt_cache: true,
            },
            Some(ModelPrice {
                input_per_million_usd: 1.0,
                output_per_million_usd: 5.0,
            }),
        ),
        entry(
            "gpt-4o",
            ProviderType::OpenAi,
            128_000,
            16_384,
            CatalogCapabilities {
                tool_calling: true,
                reasoning: false,
                prompt_cache: true,
            },
            Some(ModelPrice {
                input_per_million_usd: 2.5,
                output_per_million_usd: 10.0,
            }),
        ),
        entry(
            "gpt-4o-mini",
            ProviderType::OpenAi,
            128_000,
            16_384,
            CatalogCapabilities {
                tool_calling: true,
                reasoning: false,
                prompt_cache: true,
            },
            Some(ModelPrice {
                input_per_million_usd: 0.15,
                output_per_million_usd: 0.6,
            }),
        ),
        entry(
            "o3-mini",
            ProviderType::OpenAi,
            200_000,
            100_000,
            CatalogCapabilities {
                tool_calling: true,
                reasoning: true,
                prompt_cache: false,
            },
            Some(ModelPrice {
                input_per_million_usd: 1.1,
                output_per_million_usd: 4.4,
            }),
        ),
    ]
}

// 検出モデルの属性未確定プレースホルダ。検出 = プロバイダで応答可能なため
// availability は `Available` とする。
fn discovered_placeholder(model_id: &str) -> CatalogEntry {
    CatalogEntry {
        model_id: model_id.to_string(),
        provider: ProviderType::OpenAiCompatible,
        context_window: 0,
        max_output_tokens: 0,
        capabilities: CatalogCapabilities {
            tool_calling: false,
            reasoning: false,
            prompt_cache: false,
        },
        price: None,
        availability: Availability::Available,
        source: CatalogSource::Discovered,
        attributes_confirmed: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{
        Availability, CatalogCapabilities, CatalogSource, ModelPrice, ProviderType,
    };

    // テスト用カタログ項目。source / attributes_confirmed はマージ時の補正を
    // 確認するため、意図的に未確定の値を設定する。
    fn sample_entry(model_id: &str, availability: Availability, reasoning: bool) -> CatalogEntry {
        CatalogEntry {
            model_id: model_id.to_string(),
            provider: ProviderType::OpenAi,
            context_window: 64_000,
            max_output_tokens: 8_000,
            capabilities: CatalogCapabilities {
                tool_calling: true,
                reasoning,
                prompt_cache: false,
            },
            price: Some(ModelPrice {
                input_per_million_usd: 1.0,
                output_per_million_usd: 2.0,
            }),
            availability,
            source: CatalogSource::Builtin,
            attributes_confirmed: false,
        }
    }

    // Given: オフライン環境で組み込みカタログを生成する
    // When: 主要モデルのエントリを参照する
    // Then: ネットワークなしで属性・機能・価格を参照できる
    #[test]
    fn builtin_catalog_available_offline() {
        let catalog = ModelCatalog::builtin();

        let expected = [
            (
                "claude-sonnet-4-5",
                ProviderType::Anthropic,
                200_000_u64,
                64_000_u64,
                (true, true, true),
                (3.0_f64, 15.0_f64),
            ),
            (
                "claude-haiku-4-5",
                ProviderType::Anthropic,
                200_000,
                64_000,
                (true, false, true),
                (1.0, 5.0),
            ),
            (
                "gpt-4o",
                ProviderType::OpenAi,
                128_000,
                16_384,
                (true, false, true),
                (2.5, 10.0),
            ),
            (
                "gpt-4o-mini",
                ProviderType::OpenAi,
                128_000,
                16_384,
                (true, false, true),
                (0.15, 0.6),
            ),
            (
                "o3-mini",
                ProviderType::OpenAi,
                200_000,
                100_000,
                (true, true, false),
                (1.1, 4.4),
            ),
        ];

        for (
            model_id,
            provider,
            context_window,
            max_output_tokens,
            (tool_calling, reasoning, prompt_cache),
            (input, output),
        ) in expected
        {
            let entry = catalog.get(model_id).expect("組み込みエントリが存在する");
            assert_eq!(entry.provider, provider, "{model_id} の provider");
            assert_eq!(
                entry.context_window, context_window,
                "{model_id} の context_window"
            );
            assert_eq!(
                entry.max_output_tokens, max_output_tokens,
                "{model_id} の max_output_tokens"
            );
            assert_eq!(
                entry.capabilities.tool_calling, tool_calling,
                "{model_id} の tool_calling"
            );
            assert_eq!(
                entry.capabilities.reasoning, reasoning,
                "{model_id} の reasoning"
            );
            assert_eq!(
                entry.capabilities.prompt_cache, prompt_cache,
                "{model_id} の prompt_cache"
            );
            assert_eq!(
                entry.price,
                Some(ModelPrice {
                    input_per_million_usd: input,
                    output_per_million_usd: output,
                }),
                "{model_id} の price"
            );
            assert_eq!(
                entry.availability,
                Availability::Available,
                "{model_id} の availability"
            );
            assert_eq!(entry.source, CatalogSource::Builtin, "{model_id} の source");
            assert!(entry.attributes_confirmed, "{model_id} は属性確定済み");
        }
        assert_eq!(catalog.entries().len(), 5, "組み込みカタログは 5 項目");
    }

    // Given: 組み込みカタログと、組み込み項目を上書きする外部カタログのエントリ
    // When: merge_models_dev でマージする
    // Then: 同一 ID の項目が上書きされ、供給源と確定フラグが補正される
    #[test]
    fn merge_models_dev_overrides_builtin_entry() {
        let mut catalog = ModelCatalog::builtin();
        let mut override_entry = sample_entry("gpt-4o", Availability::Available, true);
        override_entry.context_window = 256_000;

        catalog.merge_models_dev(vec![override_entry]);

        let entry = catalog.get("gpt-4o").expect("gpt-4o が存在する");
        assert_eq!(entry.context_window, 256_000, "上書き後の属性が反映される");
        assert!(entry.capabilities.reasoning, "上書き後の機能が反映される");
        assert_eq!(
            entry.source,
            CatalogSource::ModelsDev,
            "供給源が ModelsDev になる"
        );
        assert!(entry.attributes_confirmed, "属性確定フラグが true になる");
        assert_eq!(
            catalog.entries().len(),
            5,
            "追加ではなく上書きのため項目数は不変"
        );
    }

    // Given: 組み込みカタログ
    // When: 未知のモデル ID を merge_discovered でマージする
    // Then: 属性未確定のプレースホルダとして登録される
    #[test]
    fn merge_discovered_marks_attributes_unconfirmed() {
        let mut catalog = ModelCatalog::builtin();

        catalog.merge_discovered(vec!["deepseek-chat".to_string()]);

        let entry = catalog
            .get("deepseek-chat")
            .expect("検出モデルが登録される");
        assert_eq!(entry.provider, ProviderType::OpenAiCompatible);
        assert_eq!(entry.context_window, 0);
        assert_eq!(entry.max_output_tokens, 0);
        assert!(!entry.capabilities.tool_calling);
        assert!(!entry.capabilities.reasoning);
        assert!(!entry.capabilities.prompt_cache);
        assert!(entry.price.is_none());
        assert_eq!(entry.source, CatalogSource::Discovered);
        assert!(!entry.attributes_confirmed, "検出モデルは属性未確定");
    }

    // Given: 属性確定済みの組み込みカタログ
    // When: 既存 ID を含む検出結果を merge_discovered でマージする
    // Then: 既存の確定済み項目は一切変更されない
    #[test]
    fn merge_discovered_does_not_downgrade_confirmed_entry() {
        let mut catalog = ModelCatalog::builtin();
        let before = catalog
            .get("claude-sonnet-4-5")
            .expect("claude-sonnet-4-5 が存在する")
            .clone();

        catalog.merge_discovered(vec!["claude-sonnet-4-5".to_string()]);

        let after = catalog
            .get("claude-sonnet-4-5")
            .expect("claude-sonnet-4-5 が存在する");
        assert_eq!(&before, after, "既存の確定済み項目は変更されない");
    }

    // Given: 組み込みカタログ (5 項目) と既存 ID・未知 ID の混在リスト
    // When: merge_discovered でマージする
    // Then: 未知 ID のみ挿入され、既存 ID は組み込みのまま残る
    #[test]
    fn merge_discovered_inserts_only_unknown_ids() {
        let mut catalog = ModelCatalog::builtin();

        catalog.merge_discovered(vec!["gpt-4o".to_string(), "llama-3-3-70b".to_string()]);

        assert_eq!(catalog.entries().len(), 6, "未知 ID のみ追加される");
        let discovered = catalog.get("llama-3-3-70b").expect("未知 ID が挿入される");
        assert_eq!(discovered.source, CatalogSource::Discovered);
        let existing = catalog.get("gpt-4o").expect("既存 ID が残る");
        assert_eq!(
            existing.source,
            CatalogSource::Builtin,
            "既存 ID は書き換えられない"
        );
        assert!(
            existing.attributes_confirmed,
            "既存 ID の確定フラグは維持される"
        );
    }

    // Given: 利用不可エントリと検出モデルをマージしたカタログ
    // When: 解決ヘルパー (is_available / price_of / supports) に問い合わせる
    // Then: 利用可否・価格・機能対応を正しく報告する
    #[test]
    fn resolve_helpers_report_availability_and_capability() {
        let mut catalog = ModelCatalog::builtin();
        catalog.merge_models_dev(vec![sample_entry(
            "gpt-4.1-preview",
            Availability::Unavailable,
            true,
        )]);
        catalog.merge_discovered(vec!["mystery-model".to_string()]);

        assert!(
            catalog.is_available("claude-sonnet-4-5"),
            "組み込みモデルは利用可能"
        );
        assert!(
            !catalog.is_available("gpt-4.1-preview"),
            "Unavailable なモデルは利用不可"
        );
        assert!(
            catalog.is_available("mystery-model"),
            "検出モデルは利用可能"
        );
        assert!(
            !catalog.is_available("missing-model"),
            "存在しないモデルは利用不可"
        );

        assert_eq!(
            catalog.price_of("gpt-4o"),
            Some(&ModelPrice {
                input_per_million_usd: 2.5,
                output_per_million_usd: 10.0,
            }),
            "組み込みモデルの価格を参照できる"
        );
        assert!(
            catalog.price_of("mystery-model").is_none(),
            "検出モデルは価格不明"
        );
        assert!(
            catalog.price_of("missing-model").is_none(),
            "存在しないモデルは価格なし"
        );

        assert!(catalog.supports("claude-sonnet-4-5", Capability::ToolCalling));
        assert!(catalog.supports("claude-sonnet-4-5", Capability::Reasoning));
        assert!(catalog.supports("claude-sonnet-4-5", Capability::PromptCache));
        assert!(catalog.supports("o3-mini", Capability::ToolCalling));
        assert!(catalog.supports("o3-mini", Capability::Reasoning));
        assert!(
            !catalog.supports("o3-mini", Capability::PromptCache),
            "o3-mini はプロンプトキャッシュ非対応"
        );
        assert!(
            !catalog.supports("gpt-4o", Capability::Reasoning),
            "gpt-4o は推論非対応"
        );
        assert!(
            !catalog.supports("missing-model", Capability::ToolCalling),
            "存在しないモデルは非対応"
        );
    }
}
