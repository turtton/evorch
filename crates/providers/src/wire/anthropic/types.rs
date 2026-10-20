use serde::{Deserialize, Serialize};

/// Anthropic Messages API のリクエスト本文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMessagesRequest {
    /// モデル識別子。
    pub model: String,
    /// 最大出力トークン数。
    pub max_tokens: u64,
    /// トップレベルのシステムプロンプト。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system: Option<Vec<WireContentBlock>>,
    /// user / assistant の会話履歴。
    pub messages: Vec<WireMessage>,
    /// 呼び出し可能なツール定義。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    /// 生成温度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// ストリーミングを要求するか。
    pub stream: bool,
}

/// Anthropic Messages API のメッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMessage {
    /// `user` または `assistant`。
    pub role: WireRole,
    /// メッセージのコンテンツブロック列。
    pub content: Vec<WireContentBlock>,
}

/// Anthropic Messages API のメッセージ role。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WireRole {
    /// ユーザー入力。
    User,
    /// アシスタント応答。
    Assistant,
}

/// Anthropic Messages API のコンテンツブロック。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[serde(try_from = "serde_json::Value")]
pub enum WireContentBlock {
    Image {
        source: WireImageSource,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// 平文テキスト。
    Text {
        /// 本文。
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Anthropic の extended thinking ブロック。
    Thinking {
        /// 思考本文。
        thinking: String,
    },
    /// ツール呼び出し。
    ToolUse {
        /// 呼び出し識別子。
        id: String,
        /// ツール名。
        name: String,
        /// 入力 JSON。
        input: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// ツール実行結果。
    ToolResult {
        /// 対応するツール呼び出し識別子。
        tool_use_id: String,
        /// 実行結果のブロック列。
        #[serde(default)]
        content: Vec<WireToolResultContent>,
        /// ツール実行がエラー終了したか。
        #[serde(default)]
        is_error: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        cache_control: Option<CacheControl>,
    },
    /// Complete provider block. Internal replay binding is never sent to the API.
    #[serde(untagged)]
    Raw {
        #[serde(flatten)]
        block: serde_json::Value,
        #[serde(skip)]
        prefix_hash: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheControl {
    Ephemeral,
    EphemeralHour,
}

impl Serialize for CacheControl {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let hourly = matches!(self, Self::EphemeralHour);
        let mut value = serializer.serialize_struct("CacheControl", if hourly { 2 } else { 1 })?;
        value.serialize_field("type", "ephemeral")?;
        if hourly {
            value.serialize_field("ttl", "1h")?;
        }
        value.end()
    }
}

impl<'de> Deserialize<'de> for CacheControl {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Marker {
            #[serde(rename = "type")]
            kind: String,
            ttl: Option<String>,
        }
        let marker = Marker::deserialize(deserializer)?;
        match (marker.kind.as_str(), marker.ttl.as_deref()) {
            ("ephemeral", None | Some("5m")) => Ok(Self::Ephemeral),
            ("ephemeral", Some("1h")) => Ok(Self::EphemeralHour),
            _ => Err(serde::de::Error::custom(
                "unsupported Anthropic cache marker",
            )),
        }
    }
}

impl WireContentBlock {
    pub(super) fn mark_cacheable(&mut self) -> bool {
        match self {
            Self::Image { cache_control, .. }
            | Self::Text { cache_control, .. }
            | Self::ToolUse { cache_control, .. }
            | Self::ToolResult { cache_control, .. } => {
                *cache_control = Some(CacheControl::Ephemeral);
                true
            }
            // Anthropic forbids explicit breakpoints on thinking blocks.
            Self::Thinking { .. } => false,
            Self::Raw { block, .. } => {
                if !matches!(
                    block["type"].as_str(),
                    Some("text" | "tool_use" | "tool_result" | "image")
                ) {
                    false
                } else if let Some(object) = block.as_object_mut() {
                    object.insert(
                        "cache_control".into(),
                        serde_json::json!({"type":"ephemeral"}),
                    );
                    true
                } else {
                    false
                }
            }
        }
    }
}

impl TryFrom<serde_json::Value> for WireContentBlock {
    type Error = String;

    fn try_from(block: serde_json::Value) -> Result<Self, Self::Error> {
        validate_block(&block)?;
        Ok(Self::Raw {
            block,
            prefix_hash: None,
        })
    }
}

pub(super) fn validate_block(block: &serde_json::Value) -> Result<(), String> {
    let kind = block
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "content block type must be a string".to_string())?;
    let required_strings: &[&str] = match kind {
        "text" => &["text"],
        "thinking" => &["thinking"],
        "redacted_thinking" => &["data"],
        "tool_use" | "server_tool_use" | "mcp_tool_use" => &["id", "name"],
        _ => &[],
    };
    for field in required_strings {
        if !block.get(*field).is_some_and(serde_json::Value::is_string) {
            return Err(format!("{kind}.{field} must be a string"));
        }
    }
    if kind == "thinking" && block.get("signature").is_some_and(|v| !v.is_string()) {
        return Err("thinking.signature must be a string".into());
    }
    if matches!(kind, "tool_use" | "server_tool_use" | "mcp_tool_use")
        && !block.get("input").is_some_and(serde_json::Value::is_object)
    {
        return Err(format!("{kind}.input must be an object"));
    }
    if kind == "text"
        && block
            .get("citations")
            .is_some_and(|v| !v.is_null() && !v.is_array())
    {
        return Err("text.citations must be an array or null".into());
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireImageSource {
    Base64 { media_type: String, data: String },
}

/// Anthropic のツール実行結果要素。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WireToolResultContent {
    /// 平文テキスト。
    Text {
        /// 本文。
        text: String,
    },
    Image {
        source: WireImageSource,
    },
}

/// Anthropic Messages API のツール定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    /// ツール名。
    pub name: String,
    /// ツールの説明。
    pub description: String,
    /// 入力 JSON Schema。
    pub input_schema: serde_json::Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cache_control: Option<CacheControl>,
}

/// Anthropic Messages API の非ストリーミング応答。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireMessagesResponse {
    /// メッセージ識別子。
    #[serde(default)]
    pub id: Option<String>,
    /// 応答種別。通常は `message`。
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    /// 応答 role。
    pub role: WireRole,
    /// 応答モデル。
    #[serde(default)]
    pub model: Option<String>,
    /// 応答コンテンツ。
    #[serde(default)]
    pub content: Vec<WireContentBlock>,
    /// Anthropic 固有の終了理由。
    #[serde(default)]
    pub stop_reason: Option<String>,
    /// 一致した停止シーケンス。
    #[serde(default)]
    pub stop_sequence: Option<String>,
    /// トークン使用量。
    #[serde(default)]
    pub usage: WireUsage,
}

/// Anthropic が返すトークン使用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireUsage {
    /// 通常の入力トークン数。
    #[serde(default)]
    pub input_tokens: u64,
    /// 出力トークン数。
    #[serde(default)]
    pub output_tokens: u64,
    /// キャッシュ作成に使われた入力トークン数。
    #[serde(default)]
    pub cache_creation_input_tokens: Option<u64>,
    /// キャッシュから読み出した入力トークン数。
    #[serde(default)]
    pub cache_read_input_tokens: Option<u64>,
    /// 出力トークンの内訳。thinking-token-count beta で報告される。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_tokens_details: Option<WireOutputTokensDetails>,
}

/// 出力トークンの内訳。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireOutputTokensDetails {
    /// thinking に使われた出力トークン数。
    #[serde(default)]
    pub thinking_tokens: Option<u64>,
}
