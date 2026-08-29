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
    pub system: Option<String>,
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
pub enum WireContentBlock {
    /// 平文テキスト。
    Text {
        /// 本文。
        text: String,
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
    },
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
}
