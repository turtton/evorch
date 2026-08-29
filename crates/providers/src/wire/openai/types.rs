use serde::{Deserialize, Serialize};

/// Chat Completions リクエスト本文。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireChatRequest {
    /// モデル識別子。
    pub model: String,
    /// OpenAI 形式の会話履歴。
    pub messages: Vec<WireMessage>,
    /// 利用可能な function tool。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<WireTool>,
    /// 生成温度。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 最大生成トークン数。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// SSE ストリーミングを有効にするか。
    pub stream: bool,
    /// ストリーム固有設定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream_options: Option<WireStreamOptions>,
}

/// ストリーム固有設定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireStreamOptions {
    /// 最終 usage-only chunk を要求するか。
    pub include_usage: bool,
}

/// Chat Completions の role ごとのメッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase")]
pub enum WireMessage {
    /// system メッセージ。
    System {
        /// メッセージ内容。
        content: WireContent,
    },
    /// user メッセージ。
    User {
        /// メッセージ内容。
        content: WireContent,
    },
    /// assistant メッセージ。
    Assistant {
        /// 平文内容。tool call のみなら省略できる。
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<WireContent>,
        /// assistant が要求した function call。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<WireToolCall>,
    },
    /// tool 実行結果メッセージ。
    Tool {
        /// 実行結果の内容。
        content: WireContent,
        /// 対応する tool call 識別子。
        tool_call_id: String,
    },
}

/// OpenAI が許容する文字列または text part 配列。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum WireContent {
    /// 単一文字列。
    Text(String),
    /// 複数の text part。
    Parts(Vec<WireTextPart>),
}

/// 配列形式 content の text part。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTextPart {
    /// part 種別。Chat Completions では `text`。
    #[serde(rename = "type")]
    pub kind: String,
    /// part 本文。
    pub text: String,
}

/// リクエストで宣言する tool。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireTool {
    /// tool 種別。function tool では `function`。
    #[serde(rename = "type")]
    pub kind: String,
    /// function 定義。
    pub function: WireFunctionDefinition,
}

/// リクエストで宣言する function 定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFunctionDefinition {
    /// function 名。
    pub name: String,
    /// function の説明。
    pub description: String,
    /// 引数の JSON Schema。
    pub parameters: serde_json::Value,
}

/// assistant が要求した function tool call。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireToolCall {
    /// tool call 識別子。
    pub id: String,
    /// tool 種別。function tool では `function`。
    #[serde(rename = "type")]
    pub kind: String,
    /// 呼び出す function と JSON 引数文字列。
    pub function: WireFunction,
}

/// tool call の function 情報。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WireFunction {
    /// function 名。
    pub name: String,
    /// JSON オブジェクトを表す文字列。
    pub arguments: String,
}
