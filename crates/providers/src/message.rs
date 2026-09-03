//! プロバイダ非依存の canonical メッセージモデルを定義します。

use serde::{Deserialize, Serialize};

/// 会話における発話者の役割。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// システムプロンプト。
    System,
    /// ユーザー入力。
    User,
    /// アシスタント応答。
    Assistant,
}

/// メッセージ本文を構成するコンテンツブロック。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    /// 平文テキスト。
    Text {
        /// ブロックの本文。
        text: String,
    },
    /// 思考 (reasoning) 内容。
    Reasoning {
        /// 思考の本文。
        text: String,
    },
    /// ツール呼び出し要求。
    ToolUse {
        /// 呼び出し識別子。
        id: String,
        /// ツール名。
        name: String,
        /// ツールへの入力 (JSON 値)。
        input: serde_json::Value,
    },
    /// ツール実行結果。
    ToolResult {
        /// 対応する [`ContentBlock::ToolUse`] の識別子。
        tool_call_id: String,
        /// 実行結果の内容。
        content: Vec<ToolResultContent>,
        /// ツール実行がエラー終了したかどうか。
        is_error: bool,
    },
}

/// ツール実行結果の内容要素。将来の拡張に備えた enum。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToolResultContent {
    /// 平文テキスト。
    Text {
        /// 内容の本文。
        text: String,
    },
}

/// 1 つの発話メッセージ。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// 発話者の役割。
    pub role: Role,
    /// 本文を構成するブロック列。
    pub content: Vec<ContentBlock>,
}

/// 呼び出し可能なツールの定義。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    /// ツール名。
    pub name: String,
    /// ツールの説明。
    pub description: String,
    /// 入力の JSON Schema。
    pub input_schema: serde_json::Value,
}

/// トークン使用量。
///
/// 数値はプロバイダが報告した生値であり、推定値は含まない。
/// プロバイダごとの対応:
///
/// - OpenAI: `prompt_tokens` → [`Usage::input_tokens`]、
///   `prompt_tokens_details.cached_tokens` → [`Usage::cache_read_tokens`]、
///   [`Usage::cache_write_tokens`] は常に 0。
/// - Anthropic: `input_tokens` / `output_tokens` / `cache_read_input_tokens` /
///   `cache_creation_input_tokens` をそれぞれ同名フィールドへ対応させる。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// プロンプト側の入力トークン数。
    pub input_tokens: u64,
    /// 生成された出力トークン数。
    pub output_tokens: u64,
    /// キャッシュ読み出しで賄われたトークン数。
    pub cache_read_tokens: u64,
    /// キャッシュへの書き込み (作成) トークン数。
    pub cache_write_tokens: u64,
}

/// 応答の終了理由。
///
/// プロバイダ固有の未知の値は [`FinishReason::Other`] に吸収される。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    /// 自然な完了。
    Stop,
    /// トークン数上限に到達。
    Length,
    /// ツール呼び出しによる終了。
    ToolUse,
    /// コンテンツフィルタによる停止。
    ContentFilter,
    /// 上記以外のプロバイダ固有の理由。
    Other(String),
}

/// 観測相関のためのコンテキスト。
///
/// wire プロトコルには搭載されない内部メタデータであり、プロバイダ
/// リクエスト attempt の観測イベント ([`RequestStarted`] / `FirstTokenObserved`
/// / `RequestCompleted` / `RequestFailed` — `event_bus` crate 参照) へ
/// `run_id` を相関させるためのもの。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationContext {
    /// 相関先の agent run ID。
    pub run_id: String,
}

/// チャット完了リクエスト。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    /// モデル識別子。
    pub model: String,
    /// 会話履歴。
    pub messages: Vec<Message>,
    /// 呼び出し可能なツール定義。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSpec>,
    /// 生成の温度。未指定ならプロバイダ既定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    /// 最大出力トークン数。未指定ならプロバイダ既定。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    /// 観測相関コンテキスト。wire へは送信されない。
    #[serde(default, skip_serializing)]
    pub observation: Option<ObservationContext>,
}

/// チャット完了レスポンス。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    /// アシスタントの応答メッセージ。
    pub message: Message,
    /// トークン使用量。
    pub usage: Usage,
    /// 終了理由。
    pub finish_reason: FinishReason,
}

/// プロバイダが対応する機能フラグ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderCapabilities {
    /// ストリーミングに対応するか。
    pub streaming: bool,
    /// ツール呼び出しに対応するか。
    pub tool_use: bool,
    /// 思考 (reasoning) 出力に対応するか。
    pub reasoning: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Given: Text と ToolResult を含む Message / When: JSON 化して復元 / Then: canonical フィールド名と内容が保存される
    #[test]
    fn message_round_trip_preserves_canonical_field_names() {
        let message = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: "こんにちは".to_string(),
                },
                ContentBlock::ToolResult {
                    tool_call_id: "call_1".to_string(),
                    content: vec![ToolResultContent::Text {
                        text: "42".to_string(),
                    }],
                    is_error: false,
                },
            ],
        };

        let json = serde_json::to_value(&message).unwrap();

        assert_eq!(json["role"], "user");
        assert_eq!(json["content"][0]["type"], "text");
        assert_eq!(json["content"][0]["text"], "こんにちは");
        assert_eq!(json["content"][1]["type"], "tool_result");
        assert_eq!(json["content"][1]["tool_call_id"], "call_1");
        assert_eq!(json["content"][1]["is_error"], false);
        let restored: Message = serde_json::from_value(json).unwrap();
        assert_eq!(restored, message);
    }

    // Given: ToolUse ブロック / When: JSON 化して復元 / Then: input が JSON 値として保存される
    #[test]
    fn tool_use_round_trip_preserves_input_value() {
        let block = ContentBlock::ToolUse {
            id: "call_9".to_string(),
            name: "get_weather".to_string(),
            input: json!({ "city": "tokyo", "days": 3 }),
        };

        let json = serde_json::to_value(&block).unwrap();

        assert_eq!(json["type"], "tool_use");
        assert_eq!(json["id"], "call_9");
        assert_eq!(json["name"], "get_weather");
        assert_eq!(json["input"]["city"], "tokyo");
        let restored: ContentBlock = serde_json::from_value(json).unwrap();
        assert_eq!(restored, block);
    }

    // Given: Usage / When: JSON 化 / Then: 4 つの canonical トークンフィールド名で出力される
    #[test]
    fn usage_round_trip_uses_canonical_field_names() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 0,
        };

        let json = serde_json::to_value(usage).unwrap();

        assert_eq!(
            json,
            json!({
                "input_tokens": 10,
                "output_tokens": 5,
                "cache_read_tokens": 3,
                "cache_write_tokens": 0
            })
        );
        let restored: Usage = serde_json::from_value(json).unwrap();
        assert_eq!(restored, usage);
    }

    // Given: 全フィールドを指定した ChatRequest / When: JSON 化して復元 / Then: canonical フィールド名で往復する
    #[test]
    fn chat_request_round_trip_uses_canonical_field_names() {
        let request = ChatRequest {
            model: "gpt-test".to_string(),
            messages: vec![Message {
                role: Role::System,
                content: vec![ContentBlock::Text {
                    text: "sys".to_string(),
                }],
            }],
            tools: vec![ToolSpec {
                name: "get_weather".to_string(),
                description: "天気を取得する".to_string(),
                input_schema: json!({ "type": "object" }),
            }],
            temperature: Some(0.7),
            max_tokens: Some(256),
            observation: None,
        };

        let json = serde_json::to_value(&request).unwrap();

        for key in ["model", "messages", "tools", "temperature", "max_tokens"] {
            assert!(json.get(key).is_some(), "missing key: {key}");
        }
        assert_eq!(json["tools"][0]["name"], "get_weather");
        let restored: ChatRequest = serde_json::from_value(json).unwrap();
        assert_eq!(restored, request);
    }

    // Given: ChatResponse / When: JSON 化して復元 / Then: message/usage/finish_reason が保存される
    #[test]
    fn chat_response_round_trip_preserves_fields() {
        let response = ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "answer".to_string(),
                }],
            },
            usage: Usage::default(),
            finish_reason: FinishReason::Stop,
        };

        let json = serde_json::to_value(&response).unwrap();

        assert!(json.get("message").is_some());
        assert!(json.get("usage").is_some());
        assert!(json.get("finish_reason").is_some());
        let restored: ChatResponse = serde_json::from_value(json).unwrap();
        assert_eq!(restored, response);
    }

    // Given: 未知の終了理由を含む FinishReason::Other / When: JSON 化して復元 / Then: 文字列がそのまま保存される
    #[test]
    fn finish_reason_other_round_trips_unknown_value() {
        let reason = FinishReason::Other("policy_violation".to_string());

        let json = serde_json::to_value(&reason).unwrap();

        assert_eq!(json, json!({ "other": "policy_violation" }));
        let restored: FinishReason = serde_json::from_value(json).unwrap();
        assert_eq!(restored, reason);
    }

    // Given: 各 Role / When: JSON 化 / Then: 小文字で出力される
    #[test]
    fn role_serializes_as_lowercase() {
        assert_eq!(serde_json::to_value(Role::System).unwrap(), "system");
        assert_eq!(serde_json::to_value(Role::User).unwrap(), "user");
        assert_eq!(serde_json::to_value(Role::Assistant).unwrap(), "assistant");
    }
}
