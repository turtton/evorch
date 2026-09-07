use serde::{Deserialize, Serialize};

/// OpenAI-compatible model list response.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireModelList {
    /// Response object type, omitted by some compatible servers.
    #[serde(default)]
    pub object: String,
    /// Available models in server-provided order.
    #[serde(default)]
    pub data: Vec<WireModel>,
}

/// Model entry; additional provider-specific metadata is ignored.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireModel {
    /// Model identifier accepted by completion requests.
    pub id: String,
}
