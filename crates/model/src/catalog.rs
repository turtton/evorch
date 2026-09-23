//! 外部カタログ (models.dev) とプロバイダ検出モデルを統合するモデルカタログです。

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
    /// Incremental response streaming.
    Streaming,
    /// Image input understanding.
    Vision,
    /// プロンプトキャッシュ。
    PromptCache,
}

/// モデルカタログ。
///
/// モデル ID をキーにしたカタログ項目の集合です (ADR 0013)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ModelCatalog {
    /// モデル ID をキーとしたカタログ項目。
    entries: BTreeMap<String, CatalogEntry>,
}

impl ModelCatalog {
    /// カタログ項目のマップへの参照を返す。
    pub fn entries(&self) -> &BTreeMap<String, CatalogEntry> {
        &self.entries
    }

    /// 外部ソースまたはプロバイダ検出で埋める空のカタログを生成する。
    pub fn new() -> Self {
        Self::default()
    }

    /// models.dev 等の外部カタログ取得結果をマージする。
    ///
    /// 同一モデル ID の既存項目は上書きします。マージされた項目は
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
        self.capability_support(model_id, capability).is_supported()
    }
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
            source: CatalogSource::Discovered,
            attributes_confirmed: false,
        }
    }

    #[test]
    fn new_catalog_contains_no_hardcoded_models() {
        let catalog = ModelCatalog::new();
        assert!(catalog.entries().is_empty());
        assert!(catalog.get("gpt-6-astra").is_none());
        assert!(catalog.get("kimi-for-coding").is_none());
    }

    #[test]
    fn merge_models_dev_confirms_and_replaces_discovered_entry() {
        let mut catalog = ModelCatalog::new();
        catalog.merge_discovered(vec!["custom".into()]);
        let fetched = sample_entry("custom", Availability::Available, true);
        catalog.merge_models_dev(vec![fetched]);

        let entry = catalog.get("custom").expect("external entry");
        assert_eq!(catalog.entries().len(), 1);
        assert_eq!(entry.context_window, 64_000);
        assert_eq!(entry.source, CatalogSource::ModelsDev);
        assert!(entry.attributes_confirmed);
        assert!(catalog.supports("custom", Capability::ToolCalling));
        assert!(catalog.supports("custom", Capability::Reasoning));
    }

    #[test]
    fn merge_discovered_marks_attributes_unconfirmed() {
        let mut catalog = ModelCatalog::new();
        catalog.merge_discovered(vec!["deepseek-chat".to_string()]);

        let entry = catalog.get("deepseek-chat").expect("discovered model");
        assert_eq!(entry.provider, ProviderType::OpenAiCompatible);
        assert_eq!(entry.context_window, 0);
        assert_eq!(entry.max_output_tokens, 0);
        assert!(entry.price.is_none());
        assert_eq!(entry.source, CatalogSource::Discovered);
        assert!(!entry.attributes_confirmed);
        assert!(catalog.is_available("deepseek-chat"));
        assert!(!catalog.supports("deepseek-chat", Capability::ToolCalling));
    }

    #[test]
    fn merge_discovered_does_not_downgrade_confirmed_entry() {
        let mut catalog = ModelCatalog::new();
        catalog.merge_models_dev(vec![sample_entry("custom", Availability::Available, true)]);
        let before = catalog.get("custom").expect("external entry").clone();
        catalog.merge_discovered(vec!["custom".to_string(), "unknown".to_string()]);

        assert_eq!(catalog.get("custom"), Some(&before));
        assert_eq!(catalog.entries().len(), 2);
        assert_eq!(
            catalog.get("unknown").unwrap().source,
            CatalogSource::Discovered
        );
    }

    #[test]
    fn resolve_helpers_report_source_data_and_missing_values() {
        let mut catalog = ModelCatalog::new();
        catalog.merge_models_dev(vec![
            sample_entry("available", Availability::Available, false),
            sample_entry("unavailable", Availability::Unavailable, true),
        ]);
        catalog.merge_discovered(vec!["unknown".into()]);

        assert!(catalog.is_available("available"));
        assert!(!catalog.is_available("unavailable"));
        assert!(catalog.is_available("unknown"));
        assert!(!catalog.is_available("absent"));
        assert_eq!(
            catalog.price_of("available").unwrap().input_per_million_usd,
            1.0
        );
        assert!(catalog.price_of("unknown").is_none());
        assert!(catalog.price_of("absent").is_none());
        assert!(catalog.supports("available", Capability::ToolCalling));
        assert!(!catalog.supports("available", Capability::Reasoning));
        assert!(!catalog.supports("unknown", Capability::ToolCalling));
        assert!(!catalog.supports("absent", Capability::ToolCalling));
    }
}
