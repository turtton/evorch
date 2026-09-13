//! Canonical model capability knowledge, with legacy catalog compatibility.

use serde::{Deserialize, Deserializer, Serialize};

use crate::{Capability, CatalogCapabilities, CatalogEntry, ModelCatalog};

/// Whether a source explicitly declares support for a capability.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySupport {
    /// No declaration is available.
    #[default]
    Unknown,
    /// The source explicitly declares the capability unsupported.
    Unsupported,
    /// The source explicitly declares the capability supported.
    Supported,
}

impl CapabilitySupport {
    /// Safely degrades unknown knowledge to disabled functionality.
    pub const fn is_supported(self) -> bool {
        match self {
            Self::Supported => true,
            Self::Unknown | Self::Unsupported => false,
        }
    }
}

impl From<bool> for CapabilitySupport {
    fn from(supported: bool) -> Self {
        if supported {
            Self::Supported
        } else {
            Self::Unsupported
        }
    }
}

impl<'de> Deserialize<'de> for CapabilitySupport {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum State {
            Unknown,
            Unsupported,
            Supported,
        }
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum WireSupport {
            Legacy(bool),
            State(State),
        }
        Ok(match WireSupport::deserialize(deserializer)? {
            WireSupport::Legacy(value) => value.into(),
            WireSupport::State(State::Unknown) => Self::Unknown,
            WireSupport::State(State::Unsupported) => Self::Unsupported,
            WireSupport::State(State::Supported) => Self::Supported,
        })
    }
}

/// Model-specific declarations; omitted dimensions are unknown, never unsupported.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelCapabilities {
    /// Function/tool calling.
    pub tool_calling: CapabilitySupport,
    /// Extended reasoning.
    pub reasoning: CapabilitySupport,
    /// Incremental response streaming.
    pub streaming: CapabilitySupport,
    /// Image input understanding.
    pub vision: CapabilitySupport,
    /// Prompt caching.
    pub prompt_cache: CapabilitySupport,
}

impl ModelCapabilities {
    /// Returns the tri-state declaration for a dimension.
    pub const fn support(&self, dimension: Capability) -> CapabilitySupport {
        match dimension {
            Capability::ToolCalling => self.tool_calling,
            Capability::Reasoning => self.reasoning,
            Capability::Streaming => self.streaming,
            Capability::Vision => self.vision,
            Capability::PromptCache => self.prompt_cache,
        }
    }

    /// Returns true only for explicitly supported functionality.
    pub const fn is_supported(&self, dimension: Capability) -> bool {
        self.support(dimension).is_supported()
    }
}

impl From<CatalogCapabilities> for ModelCapabilities {
    /// Converts explicit legacy declarations, not unconfirmed placeholders.
    fn from(legacy: CatalogCapabilities) -> Self {
        Self {
            tool_calling: legacy.tool_calling.into(),
            reasoning: legacy.reasoning.into(),
            prompt_cache: legacy.prompt_cache.into(),
            ..Self::default()
        }
    }
}

impl CatalogEntry {
    /// Returns canonical knowledge without treating placeholder bools as declarations.
    ///
    /// Use this rather than converting `capabilities` directly when the entry's
    /// attributes may be unconfirmed. The persisted legacy bool shape is unchanged.
    pub fn model_capabilities(&self) -> ModelCapabilities {
        if self.attributes_confirmed {
            self.capabilities.into()
        } else {
            ModelCapabilities::default()
        }
    }
}

impl ModelCatalog {
    /// Returns unknown for absent models or unconfirmed model attributes.
    pub fn capability_support(&self, model_id: &str, dimension: Capability) -> CapabilitySupport {
        self.get(model_id)
            .map_or(CapabilitySupport::Unknown, |entry| {
                entry.model_capabilities().support(dimension)
            })
    }
}
