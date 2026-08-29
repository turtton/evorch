use serde::{Deserialize, Serialize};

use super::types::WireToolCall;

/// 非ストリーミング Chat Completions レスポンス。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireChatResponse {
    /// 応答識別子。
    #[serde(default)]
    pub id: Option<String>,
    /// 生成候補。
    #[serde(default)]
    pub choices: Vec<WireChoice>,
    /// トークン使用量。
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// 非ストリーミング応答の生成候補。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireChoice {
    /// 候補インデックス。
    #[serde(default)]
    pub index: Option<usize>,
    /// assistant 応答。
    #[serde(default)]
    pub message: Option<WireResponseMessage>,
    /// 生成終了理由。
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// 非ストリーミング応答の assistant メッセージ。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireResponseMessage {
    /// role 名。
    #[serde(default)]
    pub role: Option<String>,
    /// 平文内容。
    #[serde(default)]
    pub content: Option<String>,
    /// function tool calls。
    #[serde(default)]
    pub tool_calls: Vec<WireToolCall>,
}

/// OpenAI の Chat Completions usage。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct WireUsage {
    /// 入力トークン数。
    pub prompt_tokens: u64,
    /// 出力トークン数。
    pub completion_tokens: u64,
    /// 入出力の合計トークン数。
    #[serde(default)]
    pub total_tokens: Option<u64>,
    /// 入力トークンの内訳。
    #[serde(default)]
    pub prompt_tokens_details: Option<WirePromptTokensDetails>,
}

/// OpenAI の入力トークン内訳。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub struct WirePromptTokensDetails {
    /// キャッシュから読み出された入力トークン数。
    #[serde(default)]
    pub cached_tokens: Option<u64>,
}

/// SSE の Chat Completions chunk。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireStreamChunk {
    /// 応答識別子。
    #[serde(default)]
    pub id: Option<String>,
    /// この chunk に含まれる候補差分。
    #[serde(default)]
    pub choices: Vec<WireStreamChoice>,
    /// `include_usage` による最終 chunk の使用量。
    #[serde(default)]
    pub usage: Option<WireUsage>,
}

/// ストリーミング応答の候補差分。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireStreamChoice {
    /// 候補インデックス。
    #[serde(default)]
    pub index: Option<usize>,
    /// メッセージ差分。
    #[serde(default)]
    pub delta: WireStreamDelta,
    /// この候補の生成終了理由。
    #[serde(default)]
    pub finish_reason: Option<String>,
}

/// ストリーミング assistant メッセージの差分。
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct WireStreamDelta {
    /// 最初の chunk に現れ得る role。
    #[serde(default)]
    pub role: Option<String>,
    /// 平文の追記差分。
    #[serde(default)]
    pub content: Option<String>,
    /// function tool call の追記差分。
    #[serde(default)]
    pub tool_calls: Vec<WireStreamToolCall>,
}

/// ストリーミング function tool call の差分。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WireStreamToolCall {
    /// tool call を再構築するためのインデックス。
    pub index: usize,
    /// 最初の断片に現れ得る tool call 識別子。
    #[serde(default)]
    pub id: Option<String>,
    /// tool 種別。
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    /// function 名と引数の差分。
    #[serde(default)]
    pub function: WireStreamFunction,
}

/// ストリーミング function 情報の差分。
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct WireStreamFunction {
    /// 最初の断片に現れ得る function 名。
    #[serde(default)]
    pub name: Option<String>,
    /// JSON 引数文字列の追記差分。
    #[serde(default)]
    pub arguments: String,
}
