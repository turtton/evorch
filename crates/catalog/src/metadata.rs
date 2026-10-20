use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Deserialize)]
pub(crate) struct Provider {
    #[serde(default)]
    pub models: BTreeMap<String, ModelMetadata>,
}

#[derive(Debug, Deserialize)]
#[serde(from = "WireModel")]
pub struct ModelMetadata {
    pub id: String,
    pub context_window: Option<u64>,
    pub max_input_tokens: Option<u64>,
    pub max_output_tokens: Option<u64>,
    /// USD per million input tokens.
    pub input_price: Option<f64>,
    /// USD per million output tokens.
    pub output_price: Option<f64>,
    /// USD per million cache-read tokens.
    pub cache_read_price: Option<f64>,
    /// USD per million cache-write tokens.
    pub cache_write_price: Option<f64>,
    /// `cost.tiers` of the `context` type; other tier types are ignored.
    pub price_tiers: Vec<PriceTier>,
    /// `experimental.modes.fast.cost`, the prices of priority processing.
    pub fast_pricing: Option<ModePricing>,
    pub tool_call: Option<bool>,
    pub reasoning: Option<bool>,
    pub modalities: Modalities,
    /// models.dev's provider-independent identity, such as `moonshotai/kimi-k3`.
    pub canonical_model_id: Option<String>,
}

/// USD per million tokens. Each `None` is unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Prices {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

/// Prices of a whole request whose input tokens exceed `above_input_tokens`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PriceTier {
    pub above_input_tokens: u64,
    pub prices: Prices,
}

/// Prices of an alternative processing mode, with its own input-size tiers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModePricing {
    pub prices: Prices,
    pub tiers: Vec<PriceTier>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Modalities {
    pub input: Vec<String>,
    pub output: Vec<String>,
}

#[derive(Deserialize)]
struct WireModel {
    id: String,
    #[serde(default)]
    limit: Limits,
    #[serde(default)]
    cost: Cost,
    // Experimental fields change shape freely, so unreadable parts are dropped instead of
    // failing the whole catalog.
    #[serde(default)]
    experimental: serde_json::Value,
    tool_call: Option<bool>,
    reasoning: Option<bool>,
    #[serde(default)]
    modalities: Modalities,
    canonical_model_id: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Limits {
    context: Option<u64>,
    input: Option<u64>,
    output: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Cost {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    #[serde(deserialize_with = "context_tiers")]
    tiers: Vec<PriceTier>,
}

impl Cost {
    fn prices(&self) -> Prices {
        Prices {
            input: self.input,
            output: self.output,
            cache_read: self.cache_read,
            cache_write: self.cache_write,
        }
    }
}

#[derive(Deserialize)]
struct WireTier {
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tier: TierCondition,
}

#[derive(Deserialize)]
struct TierCondition {
    #[serde(rename = "type")]
    kind: String,
    size: u64,
}

/// Keeps readable `context` tiers and drops the rest, like unknown tier types.
fn context_tiers<'de, D>(deserializer: D) -> Result<Vec<PriceTier>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(value
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|tier| WireTier::deserialize(tier).ok())
        .filter(|tier| tier.tier.kind == "context")
        .map(|tier| PriceTier {
            above_input_tokens: tier.tier.size,
            prices: Prices {
                input: tier.input,
                output: tier.output,
                cache_read: tier.cache_read,
                cache_write: tier.cache_write,
            },
        })
        .collect())
}

fn fast_pricing(experimental: &serde_json::Value) -> Option<ModePricing> {
    let cost = Cost::deserialize(experimental.pointer("/modes/fast/cost")?).ok()?;
    let pricing = ModePricing {
        prices: cost.prices(),
        tiers: cost.tiers,
    };
    (pricing != ModePricing::default()).then_some(pricing)
}

impl From<WireModel> for ModelMetadata {
    fn from(model: WireModel) -> Self {
        Self {
            id: model.id,
            context_window: model.limit.context,
            max_input_tokens: model.limit.input,
            max_output_tokens: model.limit.output,
            input_price: model.cost.input,
            output_price: model.cost.output,
            cache_read_price: model.cost.cache_read,
            cache_write_price: model.cost.cache_write,
            fast_pricing: fast_pricing(&model.experimental),
            price_tiers: model.cost.tiers,
            tool_call: model.tool_call,
            reasoning: model.reasoning,
            modalities: model.modalities,
            canonical_model_id: model.canonical_model_id.filter(|id| !id.trim().is_empty()),
        }
    }
}
