//! 永続化するエンティティを定義します。
use std::fmt;
use std::time::SystemTime;

use event_bus::{
    AgentMessageEvent, CompactionEvent, EventKind, LifecycleEvent, MessageEvent, ProviderEvent,
    ToolEvent,
};

use crate::error::{SecretRule, StorageError};

macro_rules! string_enum {
    ($name:ident { $($variant:ident => $value:literal),+ $(,)? }) => {
        impl $name {
            /// SQLite に保存する文字列表現を返します。
            #[must_use]
            pub const fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $value,)+
                }
            }

            /// SQLite の文字列表現から値を復元します。
            #[must_use]
            #[allow(
                clippy::should_implement_trait,
                reason = "the storage contract requires this Option-returning inherent method"
            )]
            pub fn from_str(value: &str) -> Option<Self> {
                match value {
                    $($value => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = ();

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::from_str(value).ok_or(())
            }
        }
    };
}

/// セッションの永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    /// 実行中です。
    Running,
    /// 別エージェントへ委譲済みです。
    Delegated,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(SessionStatus {
    Running => "running",
    Delegated => "delegated",
    Completed => "completed",
    Failed => "failed",
});

/// タスクの永続化状態です。
///
/// V1 マイグレーションの `tasks.status` CHECK 制約が
/// `'running','completed','failed'` のみを許容するため `Cancelled` を持たず、
/// キャンセルイベントは射影で [`TaskStatus::Failed`] へ写像されます。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 実行中です。
    Running,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(TaskStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
});

/// エージェント実行の永続化状態です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentRunStatus {
    /// 実行中です。
    Running,
    /// 正常に完了しました。
    Completed,
    /// 失敗しました。
    Failed,
}

string_enum!(AgentRunStatus {
    Running => "running",
    Completed => "completed",
    Failed => "failed",
});

/// メッセージの送信主体です。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    /// ユーザーからのメッセージです。
    User,
    /// アシスタントからのメッセージです。
    Assistant,
    /// システムからのメッセージです。
    System,
    /// ツールからのメッセージです。
    Tool,
}

string_enum!(MessageRole {
    User => "user",
    Assistant => "assistant",
    System => "system",
    Tool => "tool",
});

/// セッションの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRecord {
    /// セッション識別子です。
    pub id: String,
    /// 親セッション識別子です。
    pub parent_id: Option<String>,
    /// セッション状態です。
    pub status: SessionStatus,
    /// 失敗理由です。
    pub failure_reason: Option<String>,
    /// 委譲先です。
    pub delegated_to: Option<String>,
    /// 保存済みイベントの累積バイト数です。
    pub total_event_bytes: u64,
    /// 作成日時です。
    pub created_at: SystemTime,
    /// 更新日時です。
    pub updated_at: SystemTime,
}

/// タスクの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRecord {
    /// タスク識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: Option<String>,
    /// タスク状態です。
    pub status: TaskStatus,
    /// 作成日時です。
    pub created_at: SystemTime,
    /// 更新日時です。
    pub updated_at: SystemTime,
}

/// メッセージの永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRecord {
    /// メッセージ識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: String,
    /// 送信主体です。
    pub role: MessageRole,
    /// メッセージ本文です。
    pub content: String,
    /// 推論内容です。
    pub reasoning: Option<String>,
    /// 作成日時です。
    pub created_at: SystemTime,
    /// 更新日時です。
    pub updated_at: SystemTime,
}

/// エージェント実行の永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRunRecord {
    /// 実行識別子です。
    pub id: String,
    /// 所属セッション識別子です。
    pub session_id: String,
    /// プロバイダー名です。
    pub provider: String,
    /// モデル名です。
    pub model: String,
    /// 実行状態です。
    pub status: AgentRunStatus,
    /// 開始日時です。
    pub started_at: SystemTime,
    /// 終了日時です。
    pub finished_at: Option<SystemTime>,
}

/// カタログ更新の永続化レコードです。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogUpdateRecord {
    /// 更新元を識別する文字列です。
    pub source: String,
    /// 更新後のモデル数です。
    pub model_count: u32,
    /// 更新の詳細です。
    pub detail: String,
    /// 更新を記録した Unix epoch ナノ秒です。
    pub recorded_at_ns: i64,
}

/// guard が既知 credential 値として取り込む環境変数名の限定リストです。
///
/// これ以外の環境変数は読みません。値そのものは診断・ログ・[`fmt::Debug`] 出力へ
/// 一切出さず、比較の内部処理にのみ使用します。
pub(crate) const CREDENTIAL_ENV_NAMES: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "GOOGLE_API_KEY",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "MISTRAL_API_KEY",
    "COHERE_API_KEY",
    "HF_TOKEN",
    "AWS_SECRET_ACCESS_KEY",
    "SLACK_BOT_TOKEN",
    "SLACK_APP_TOKEN",
];

/// 既知 credential 値として扱う最小長です。短すぎる値による通常文の過剰拒否を
/// 防ぎます。
const MIN_KNOWN_VALUE_LEN: usize = 8;

/// 永続化 ingress の heuristic secret guard です。
///
/// ADR 0008 の credential 隔離を補強する defense-in-depth であり、完全な
/// secret 非漏洩保証ではありません。検出は deterministic で、時刻・乱数へ
/// 依存しません。
pub(crate) struct SecretGuard {
    known_values: Vec<String>,
}

impl SecretGuard {
    /// 限定された credential 環境変数名から既知値を取り込んで guard を構築します。
    pub(crate) fn from_env() -> Self {
        let known_values = CREDENTIAL_ENV_NAMES
            .iter()
            .filter_map(|name| std::env::var(name).ok())
            .filter(|value| value.len() >= MIN_KNOWN_VALUE_LEN && !value.trim().is_empty())
            .collect();
        Self { known_values }
    }

    /// 既知 credential 値を明示的に注入して guard を構築します。
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "explicit constructor input is exercised by in-crate contract tests; \
                      production wiring ingests the limited credential env names"
        )
    )]
    #[must_use]
    pub(crate) fn with_known_values(values: impl IntoIterator<Item = String>) -> Self {
        let known_values = values
            .into_iter()
            .filter(|value| value.len() >= MIN_KNOWN_VALUE_LEN && !value.trim().is_empty())
            .collect();
        Self { known_values }
    }

    /// メッセージレコードの `content` / `reasoning` が永続化可能か検査します。
    pub(crate) fn check_message_record(&self, record: &MessageRecord) -> Result<(), StorageError> {
        self.check_text("message", "content", &record.content)?;
        if let Some(reasoning) = &record.reasoning {
            self.check_text("message", "reasoning", reasoning)?;
        }
        Ok(())
    }

    /// 永続化対象イベントの human-readable text（`MessageDelta` / `ReasoningDelta` の
    /// `delta` と `reason` 系 field）が serialize / INSERT 可能か検査します。
    pub(crate) fn check_event_kind(&self, kind: &EventKind) -> Result<(), StorageError> {
        // reason / delta 系の自由文字列 field を明示列挙する。新しい text field を持つ
        // variant が event-bus へ追加されたらここへも検査を追加すること。
        match kind {
            EventKind::Lifecycle(LifecycleEvent::Failed { reason, .. }) => {
                self.check_text("event", "Failed.reason", reason)
            }
            EventKind::Lifecycle(LifecycleEvent::AgentRunStateChanged {
                reason: Some(reason),
                ..
            }) => self.check_text("event", "AgentRunStateChanged.reason", reason),
            EventKind::Lifecycle(LifecycleEvent::RoutingDecision { reason, .. }) => {
                self.check_text("event", "RoutingDecision.reason", reason)
            }
            EventKind::Message(MessageEvent::MessageDelta { delta }) => {
                self.check_text("event", "MessageDelta.delta", delta)
            }
            EventKind::Message(MessageEvent::ReasoningDelta { delta }) => {
                self.check_text("event", "ReasoningDelta.delta", delta)
            }
            EventKind::Tool(ToolEvent::ExecutionDenied { reason, .. }) => {
                self.check_text("event", "ExecutionDenied.reason", reason)
            }
            EventKind::Provider(ProviderEvent::ProviderFallback { reason, .. }) => {
                self.check_text("event", "ProviderFallback.reason", reason)
            }
            EventKind::Provider(ProviderEvent::RequestCompleted { finish_reason, .. }) => {
                // FinishReason::Other は provider 由来の任意文字列を保持し得る
                // （providers::observe::emit_completed）。
                self.check_text("event", "RequestCompleted.finish_reason", finish_reason)
            }
            // AgentMessage の本文は MessageDelta と同様の自由文字列であり、
            // bus / storage へ流れるため fail-closed で走査する。
            EventKind::AgentMessage(AgentMessageEvent::Delivered { message, .. }) => {
                self.check_text("event", "AgentMessage.content", &message.content)
            }
            // Compaction の summary は会話由来の自由文字列であり、
            // checkpoint / run の識別子も同様に fail-closed で走査する。
            EventKind::Compaction(CompactionEvent::Compacted {
                run_id,
                checkpoint_id,
                summary,
                ..
            }) => {
                self.check_text("event", "Compacted.summary", summary)?;
                self.check_text("event", "Compacted.checkpoint_id", checkpoint_id)?;
                self.check_text("event", "Compacted.run_id", run_id)
            }
            EventKind::Lifecycle(_)
            | EventKind::Tool(_)
            | EventKind::Provider(_)
            | EventKind::Usage(_)
            | EventKind::Fault(_) => Ok(()),
            // Orchestrator は goal 本文・findings・detail 等の自由文字列を
            // payload 全体で保持するため、serialize 結果ごと fail-closed で
            // 走査する。
            EventKind::Orchestrator(event) => {
                let payload = serde_json::to_string(event)
                    .map_err(|error| StorageError::Serialization(error.to_string()))?;
                self.check_text("event", "Orchestrator.payload", &payload)
            }
        }
    }

    fn check_text(
        &self,
        entity: &'static str,
        field: &'static str,
        text: &str,
    ) -> Result<(), StorageError> {
        match detect_secret(text, &self.known_values) {
            Some((rule, _matched)) => Err(StorageError::SecretDetected {
                entity,
                field,
                rule,
            }),
            None => Ok(()),
        }
    }
}

/// secret 候補の値本体を出力へ含めないことを型で強制するため手書き実装します。
impl fmt::Debug for SecretGuard {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretGuard")
            .field(
                "known_values",
                &format_args!("<{} redacted>", self.known_values.len()),
            )
            .finish()
    }
}

/// テキスト中の credential らしき値を規則と一致部分とともに返します。
///
/// 判定は deterministic（時刻・乱数非依存）で、新規 dependency を要さない
/// 手書きマッチャで構成します。過剰拒否を避けるため、いずれの規則も
/// プロバイダ接頭辞か十分な長さ・字種の双方を要求します。
fn detect_secret<'a>(text: &'a str, known_values: &'a [String]) -> Option<(SecretRule, &'a str)> {
    if let Some(matched) = detect_known_value(text, known_values) {
        return Some((SecretRule::KnownCredentialValue, matched));
    }
    detect_key_shape(text)
}

/// 既知 credential 値の完全一致（部分文字列）を検出します。返り値は既知値側を
/// 指し、呼び出し側テキストからの切り出しを行わないため、診断へ前後
/// コンテキストが紛れ込む経路を構造的に排除します。
fn detect_known_value<'a>(text: &str, known_values: &'a [String]) -> Option<&'a str> {
    known_values
        .iter()
        .find(|value| text.contains(value.as_str()))
        .map(String::as_str)
}

const fn base64url(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_'
}

/// `prefix + 本体` 形状を走査します。接頭辞の直前が英数字の場合は語中の
/// 偶然一致（例: `ask-...` 中の `sk-`）なので棄却します。
fn scan_prefixed<'a>(
    text: &'a str,
    prefix: &str,
    min_body: usize,
    body_char: fn(u8) -> bool,
) -> Option<&'a str> {
    for (index, _) in text.match_indices(prefix) {
        if index > 0 && text.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let body_start = index + prefix.len();
        let body_len = text.as_bytes()[body_start..]
            .iter()
            .take_while(|&&byte| body_char(byte))
            .count();
        if body_len >= min_body {
            return Some(&text[index..body_start + body_len]);
        }
    }
    None
}

fn detect_key_shape(text: &str) -> Option<(SecretRule, &str)> {
    type Rule = (&'static str, &'static str, usize, fn(u8) -> bool);
    const PREFIX_RULES: &[Rule] = &[
        ("sk-", "openai-style-key", 20, base64url),
        ("ghp_", "github-token", 30, |byte| {
            byte.is_ascii_alphanumeric()
        }),
        ("gho_", "github-token", 30, |byte| {
            byte.is_ascii_alphanumeric()
        }),
        ("ghu_", "github-token", 30, |byte| {
            byte.is_ascii_alphanumeric()
        }),
        ("ghs_", "github-token", 30, |byte| {
            byte.is_ascii_alphanumeric()
        }),
        ("ghr_", "github-token", 30, |byte| {
            byte.is_ascii_alphanumeric()
        }),
        ("github_pat_", "github-pat", 22, base64url),
        ("AKIA", "aws-access-key-id", 16, |byte| {
            byte.is_ascii_uppercase() || byte.is_ascii_digit()
        }),
        ("AIza", "google-api-key", 30, base64url),
    ];
    for (prefix, label, min_body, body_char) in PREFIX_RULES {
        if let Some(matched) = scan_prefixed(text, prefix, *min_body, *body_char) {
            return Some((SecretRule::ApiKeyShape(label), matched));
        }
    }
    if let Some(matched) = detect_slack_token(text) {
        return Some((SecretRule::ApiKeyShape("slack-token"), matched));
    }
    if let Some(matched) = detect_private_key_block(text) {
        return Some((SecretRule::ApiKeyShape("private-key-block"), matched));
    }
    if let Some(matched) = detect_jwt(text) {
        return Some((SecretRule::ApiKeyShape("jwt"), matched));
    }
    None
}

/// Slack token 形状 `xox[baprs]-...` を検出します。
fn detect_slack_token(text: &str) -> Option<&str> {
    for (index, _) in text.match_indices("xox") {
        if index > 0 && text.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let bytes = &text.as_bytes()[index..];
        let body_start = index + 5;
        if !(bytes.len() > 5
            && matches!(bytes[3], b'b' | b'a' | b'p' | b'r' | b's')
            && bytes[4] == b'-')
        {
            continue;
        }
        let body_len = text.as_bytes()[body_start..]
            .iter()
            .take_while(|&&byte| byte.is_ascii_alphanumeric() || byte == b'-')
            .count();
        if body_len >= 10 {
            return Some(&text[index..body_start + body_len]);
        }
    }
    None
}

/// PEM 等の private key block ヘッダを検出します。過剰拒否を避けるため PEM
/// label 形状（`-----BEGIN ` の後は英大文字・数字・空白のみ）を要求し、
/// 一致部分は鍵本体ではなくヘッダのみとします。
fn detect_private_key_block(text: &str) -> Option<&str> {
    for (index, _) in text.match_indices("-----BEGIN ") {
        let rest = &text[index..];
        let window = &rest[..rest.len().min(80)];
        let label_and_key = &window["-----BEGIN ".len()..];
        let Some(key_at) = label_and_key.find("PRIVATE KEY") else {
            continue;
        };
        let label = &label_and_key[..key_at];
        if !label
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b' ')
        {
            continue;
        }
        let tail = &label_and_key[key_at + "PRIVATE KEY".len()..];
        let end = if tail.starts_with(" BLOCK-----") {
            key_at + "PRIVATE KEY BLOCK-----".len()
        } else if tail.starts_with("-----") {
            key_at + "PRIVATE KEY-----".len()
        } else {
            continue;
        };
        return Some(&rest[.."-----BEGIN ".len() + end]);
    }
    None
}

/// JWT 形状（`eyJ` 始まりの三区分 base64url）を検出します。
fn detect_jwt(text: &str) -> Option<&str> {
    for (index, _) in text.match_indices("eyJ") {
        if index > 0 && text.as_bytes()[index - 1].is_ascii_alphanumeric() {
            continue;
        }
        let bytes = &text.as_bytes()[index..];
        let take = |from: usize| -> usize {
            bytes[from..]
                .iter()
                .take_while(|&&byte| base64url(byte))
                .count()
        };
        let first = take(0);
        if first < 10 || bytes.get(first) != Some(&b'.') {
            continue;
        }
        let second = take(first + 1);
        if second < 16 || bytes.get(first + 1 + second) != Some(&b'.') {
            continue;
        }
        let third = take(first + second + 2);
        if third < 16 {
            continue;
        }
        return Some(&text[index..index + first + second + third + 2]);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use event_bus::{CompactionReason, FaultEvent};

    const JWT_SHAPED: &str = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJVadQssw5c";
    const KNOWN_VALUE: &str = "evorch-known-credential-fixture-value-0123456789";

    #[test]
    fn detect_rejects_representative_api_key_shapes() {
        // Given: 代表的な credential 形状を本文へ混入させたテキスト
        let cases: [(&str, &str, SecretRule); 7] = [
            (
                "leak: sk-test-evorch-9f8e7d6c5b4a3f2e1d",
                "sk-test-evorch-9f8e7d6c5b4a3f2e1d",
                SecretRule::ApiKeyShape("openai-style-key"),
            ),
            (
                "key=sk-ant-api03-aaaa-bbbb-cccc-dddd-eeee",
                "sk-ant-api03-aaaa-bbbb-cccc-dddd-eeee",
                SecretRule::ApiKeyShape("openai-style-key"),
            ),
            (
                "token ghp_0123456789abcdefghijklmnopqrstuvwxyz end",
                "ghp_0123456789abcdefghijklmnopqrstuvwxyz",
                SecretRule::ApiKeyShape("github-token"),
            ),
            (
                "bearer github_pat_11ABCDEFGH_ijklmnopqrstuvwxyz",
                "github_pat_11ABCDEFGH_ijklmnopqrstuvwxyz",
                SecretRule::ApiKeyShape("github-pat"),
            ),
            (
                "slack xoxb-1234-5678-abcdefgh",
                "xoxb-1234-5678-abcdefgh",
                SecretRule::ApiKeyShape("slack-token"),
            ),
            (
                "aws AKIAIOSFODNN7EXAMPLE",
                "AKIAIOSFODNN7EXAMPLE",
                SecretRule::ApiKeyShape("aws-access-key-id"),
            ),
            (
                "google AIzaSyAbcdefghijklmnopqrstuvwxyz01234567",
                "AIzaSyAbcdefghijklmnopqrstuvwxyz01234567",
                SecretRule::ApiKeyShape("google-api-key"),
            ),
        ];

        // When / Then: 各形状が規則ラベル付きで検出される
        for (text, matched, rule) in cases {
            assert_eq!(
                detect_secret(text, &[]),
                Some((rule, matched)),
                "text: {text}"
            );
        }
        let jwt = format!("auth: {JWT_SHAPED}");
        assert_eq!(
            detect_secret(&jwt, &[]),
            Some((SecretRule::ApiKeyShape("jwt"), JWT_SHAPED))
        );
        assert_eq!(
            detect_secret("-----BEGIN PRIVATE KEY-----\nMIIB", &[]),
            Some((
                SecretRule::ApiKeyShape("private-key-block"),
                "-----BEGIN PRIVATE KEY-----"
            ))
        );
    }

    #[test]
    fn detect_ignores_normal_prose_and_short_tokens() {
        // Given: 通常文と credential に見えるが規則を満たさない文字列群
        let negatives = [
            "hello, this is a normal message",
            "これは通常の日本語の文章です。環境変数 OPENAI_API_KEY を設定してください。",
            "abc12345",
            "ghp_short",
            "sk-x",
            "ask-this-boundary-must-not-trip-the-guard-0123456789abcdef",
            "123e4567-e89b-12d3-a456-426614174000",
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4",
            "a fragment eyJhbGciOiJIUzI1NiIs without dot segments",
            "-----BEGIN CERTIFICATE-----\nMIIB",
            "-----BEGIN not-a-pem private key-----",
            "wordAKIAIOSFODNN7EXAMPLE",
            "xsk-test-evorch-9f8e7d6c5b4a3f2e1d",
        ];

        // When / Then: いずれも検出されない
        for text in negatives {
            assert_eq!(detect_secret(text, &[]), None, "text: {text}");
        }
    }

    #[test]
    fn known_values_reject_exact_occurrence_and_never_leak_into_debug() {
        // Given: 既知 credential 値を注入した guard
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned(), "short".to_owned()]);

        // When / Then: 既知値の含有を known-credential-value 規則で検出する
        let text = format!("prefix {KNOWN_VALUE} suffix");
        let Some((rule, matched)) = detect_secret(&text, &guard.known_values) else {
            panic!("known credential value must be detected");
        };
        assert_eq!(rule, SecretRule::KnownCredentialValue);
        assert_eq!(matched, KNOWN_VALUE);
        // 最小長未満の既知値は過剰拒否防止のため取り込まない
        assert_eq!(detect_secret("note: short", &guard.known_values), None);

        // And: Debug 出力に値本体が現れない（数のみ）
        let debug = format!("{guard:?}");
        assert!(debug.contains("<1 redacted>"));
        assert!(!debug.contains(KNOWN_VALUE));
    }

    #[test]
    fn event_check_covers_reason_and_delta_fields_and_skips_typed_only_variants() {
        // Given: 既知値を注入した guard と各イベント variant
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let cases: [(&str, EventKind); 7] = [
            (
                "Failed.reason",
                LifecycleEvent::Failed {
                    session_id: "s".into(),
                    reason: format!("boom {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "AgentRunStateChanged.reason",
                LifecycleEvent::AgentRunStateChanged {
                    run_id: "r".into(),
                    from: event_bus::AgentRunPhase::Running,
                    to: event_bus::AgentRunPhase::Error,
                    reason: Some(format!("die {KNOWN_VALUE}")),
                }
                .into(),
            ),
            (
                "MessageDelta.delta",
                MessageEvent::MessageDelta {
                    delta: format!("out {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "ReasoningDelta.delta",
                MessageEvent::ReasoningDelta {
                    delta: format!("think {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "ExecutionDenied.reason",
                ToolEvent::ExecutionDenied {
                    tool_name: "t".into(),
                    call_id: "c".into(),
                    reason: format!("deny {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "ProviderFallback.reason",
                ProviderEvent::ProviderFallback {
                    from_provider: "a".into(),
                    to_provider: "b".into(),
                    reason: format!("flip {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "RequestCompleted.finish_reason",
                ProviderEvent::RequestCompleted {
                    request_id: "q".into(),
                    provider: "p".into(),
                    profile: None,
                    protocol: "proto".into(),
                    model: "m".into(),
                    streaming: true,
                    duration_ms: 1,
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 3,
                    cache_write_tokens: 4,
                    finish_reason: format!("other {KNOWN_VALUE}"),
                    run_id: None,
                }
                .into(),
            ),
        ];

        // When / Then: 各 field 名付きで拒否される
        for (field, kind) in cases {
            let Err(StorageError::SecretDetected {
                entity,
                field: actual,
                ..
            }) = guard.check_event_kind(&kind)
            else {
                panic!("field {field} must be rejected");
            };
            assert_eq!(entity, "event");
            assert_eq!(actual, field);
        }

        // And: 型付き分類のみの variant と reason 未設定の variant は検査対象外
        let skipped: [EventKind; 2] = [
            FaultEvent::SubscriberLagged {
                subscriber_id: 1,
                skipped: 2,
            }
            .into(),
            LifecycleEvent::AgentRunStateChanged {
                run_id: "r".into(),
                from: event_bus::AgentRunPhase::Running,
                to: event_bus::AgentRunPhase::Running,
                reason: None,
            }
            .into(),
        ];
        for kind in skipped {
            guard
                .check_event_kind(&kind)
                .expect("typed-only variant must pass");
        }
    }

    #[test]
    fn event_check_rejects_secret_in_compaction_fields() {
        // Given: 既知値を注入した guard と Compaction イベント
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let cases: [(&str, EventKind); 3] = [
            (
                "Compacted.summary",
                CompactionEvent::Compacted {
                    run_id: "r".into(),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: "cp".into(),
                    summary: format!("chat {KNOWN_VALUE}"),
                }
                .into(),
            ),
            (
                "Compacted.checkpoint_id",
                CompactionEvent::Compacted {
                    run_id: "r".into(),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: format!("cp {KNOWN_VALUE}"),
                    summary: "safe".into(),
                }
                .into(),
            ),
            (
                "Compacted.run_id",
                CompactionEvent::Compacted {
                    run_id: format!("run {KNOWN_VALUE}"),
                    reason: CompactionReason::Automatic,
                    threshold: 0.8,
                    context_window_tokens: 100,
                    estimated_tokens_before: 90,
                    estimated_tokens_after: 30,
                    compacted_range_start: 0,
                    compacted_range_end: 3,
                    checkpoint_id: "cp".into(),
                    summary: "safe".into(),
                }
                .into(),
            ),
        ];

        // When / Then: 各 field 名付きで拒否される
        for (field, kind) in cases {
            let Err(StorageError::SecretDetected {
                entity,
                field: actual,
                ..
            }) = guard.check_event_kind(&kind)
            else {
                panic!("field {field} must be rejected");
            };
            assert_eq!(entity, "event");
            assert_eq!(actual, field);
        }
    }

    #[test]
    fn routing_decision_reason_is_secret_guarded() {
        // Given: 既知値を注入した guard と reason へ既知 credential 値を含む
        //        RoutingDecision イベント
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let kind = EventKind::Lifecycle(LifecycleEvent::RoutingDecision {
            shape: "Direct".into(),
            reason: format!("matched {KNOWN_VALUE}"),
            source: event_bus::RoutingSource::LocalRule {
                rule: "direct-keyword:direct".into(),
            },
        });

        // When / Then: "RoutingDecision.reason" の field 名付きで拒否される
        let Err(StorageError::SecretDetected {
            entity,
            field: actual,
            ..
        }) = guard.check_event_kind(&kind)
        else {
            panic!("RoutingDecision.reason must be rejected");
        };
        assert_eq!(entity, "event");
        assert_eq!(actual, "RoutingDecision.reason");
    }

    #[test]
    fn message_record_check_reports_content_and_reasoning_fields() {
        // Given: guard と本文/推論へ既知値を含むレコード
        let guard = SecretGuard::with_known_values([KNOWN_VALUE.to_owned()]);
        let base = MessageRecord {
            id: "m".into(),
            session_id: "s".into(),
            role: MessageRole::Assistant,
            content: "safe".into(),
            reasoning: None,
            created_at: SystemTime::UNIX_EPOCH,
            updated_at: SystemTime::UNIX_EPOCH,
        };

        // When / Then: content / reasoning それぞれの field 名で拒否される
        let bad_content = MessageRecord {
            content: format!("say {KNOWN_VALUE}"),
            ..base.clone()
        };
        let Err(StorageError::SecretDetected { entity, field, .. }) =
            guard.check_message_record(&bad_content)
        else {
            panic!("content must be rejected");
        };
        assert_eq!((entity, field), ("message", "content"));

        let bad_reasoning = MessageRecord {
            reasoning: Some(format!("why {KNOWN_VALUE}")),
            ..base
        };
        let Err(StorageError::SecretDetected { entity, field, .. }) =
            guard.check_message_record(&bad_reasoning)
        else {
            panic!("reasoning must be rejected");
        };
        assert_eq!((entity, field), ("message", "reasoning"));
    }
}
