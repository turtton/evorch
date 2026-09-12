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
    pub tool_call: Option<bool>,
    pub reasoning: Option<bool>,
    pub modalities: Modalities,
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
    tool_call: Option<bool>,
    reasoning: Option<bool>,
    #[serde(default)]
    modalities: Modalities,
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
            tool_call: model.tool_call,
            reasoning: model.reasoning,
            modalities: model.modalities,
        }
    }
}
