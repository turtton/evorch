//! Shared heuristic credential redaction for persisted context and tool output.
//! Values stay private; diagnostics expose only rules and replacement counts.

use std::fmt;

/// secret guard が検出に用いた規則を識別します。
///
/// 診断には規則名のみを含め、検出対象となった値本体やその前後コンテキストは
/// 一切含みません（ADR 0008 の credential 非漏洩方針）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretRule {
    /// 明示的に注入された既知 credential 値、または限定的な credential 環境変数の
    /// 値と一致しました。
    KnownCredentialValue,
    /// 高シグナルな API key 形状（プロバイダ接頭辞、private key block 等）に
    /// 一致しました。保持するのは規則ラベルのみです。
    ApiKeyShape(&'static str),
}

impl fmt::Display for SecretRule {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::KnownCredentialValue => formatter.write_str("known-credential-value"),
            Self::ApiKeyShape(label) => formatter.write_str(label),
        }
    }
}

/// guard が既知 credential 値として取り込む環境変数名の限定リストです。
///
/// これ以外の環境変数は読みません。値そのものは診断・ログ・[`fmt::Debug`] 出力へ
/// 一切出さず、比較の内部処理にのみ使用します。
const CREDENTIAL_ENV_NAMES: &[&str] = &[
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "MOONSHOT_API_KEY",
    "KIMI_API_KEY",
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

/// An owned, sanitized copy of text and the number of removed spans.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RedactedText {
    pub text: String,
    pub count: usize,
}

/// Matches configured credential values and deterministic credential shapes.
pub struct SecretRedactor {
    known_values: Vec<String>,
}

impl SecretRedactor {
    #[must_use]
    pub fn from_env() -> Self {
        Self::with_known_values(
            CREDENTIAL_ENV_NAMES
                .iter()
                .filter_map(|name| std::env::var(name).ok()),
        )
    }

    #[must_use]
    pub fn with_known_values(values: impl IntoIterator<Item = String>) -> Self {
        let mut known_values: Vec<_> = values
            .into_iter()
            .filter(|value| value.len() >= MIN_KNOWN_VALUE_LEN && !value.trim().is_empty())
            .collect();
        known_values.sort_by_key(|value| std::cmp::Reverse(value.len()));
        known_values.dedup();
        Self { known_values }
    }

    /// Returns only a rule, never the matching credential.
    #[must_use]
    pub fn detect(&self, text: &str) -> Option<SecretRule> {
        detect_secret(text, &self.known_values).map(|(rule, _)| rule)
    }

    /// Redacts an owned copy. The caller's live text is never modified.
    #[must_use]
    pub fn redact(&self, text: &str) -> RedactedText {
        let mut spans = Vec::new();
        for known in &self.known_values {
            for (start, _) in text.match_indices(known) {
                spans.push((start, start + known.len(), SecretRule::KnownCredentialValue));
            }
        }
        for &(prefix, label, minimum, body) in PREFIX_RULES {
            collect_spans(
                text,
                |remaining| scan_prefixed(remaining, prefix, minimum, body),
                SecretRule::ApiKeyShape(label),
                &mut spans,
            );
        }
        collect_spans(
            text,
            detect_slack_token,
            SecretRule::ApiKeyShape("slack-token"),
            &mut spans,
        );
        collect_spans(text, detect_jwt, SecretRule::ApiKeyShape("jwt"), &mut spans);
        let mut offset = 0;
        while let Some(header) = detect_private_key_block(&text[offset..]) {
            let start = offset + text[offset..].find(header).expect("matched header exists");
            let label = header
                .strip_prefix("-----BEGIN ")
                .unwrap()
                .strip_suffix("-----")
                .unwrap();
            let end_marker = format!("-----END {label}-----");
            let end = text[start + header.len()..]
                .find(&end_marker)
                .map_or(text.len(), |index| {
                    start + header.len() + index + end_marker.len()
                });
            spans.push((start, end, SecretRule::ApiKeyShape("private-key-block")));
            offset = end;
        }
        // Merge overlapping matches (e.g. a configured key inside a PEM block).
        // Every matcher scans forward, avoiding quadratic rescans for secret-heavy logs.
        spans.sort_unstable_by_key(|(start, end, _)| (*start, std::cmp::Reverse(*end)));
        let mut merged: Vec<(usize, usize, SecretRule)> = Vec::new();
        for (start, end, rule) in spans {
            if let Some(last) = merged.last_mut()
                && start < last.1
            {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end, rule));
            }
        }
        let count = merged.len();
        let mut result = String::with_capacity(text.len());
        let mut offset = 0;
        for (start, end, rule) in merged {
            result.push_str(&text[offset..start]);
            result.push_str(&format!("[REDACTED:{rule}]"));
            offset = end;
        }
        result.push_str(&text[offset..]);
        RedactedText {
            text: result,
            count,
        }
    }

    /// Redact textual JSON payloads, including credential-shaped object keys.
    /// Callers must exclude protocol identifiers from this payload-only traversal.
    pub fn redact_json(&self, value: &mut serde_json::Value) -> usize {
        match value {
            serde_json::Value::String(text) => {
                let redacted = self.redact(text);
                *text = redacted.text;
                redacted.count
            }
            serde_json::Value::Array(values) => {
                values.iter_mut().map(|value| self.redact_json(value)).sum()
            }
            serde_json::Value::Object(values) => {
                let original = std::mem::take(values);
                let mut count = 0;
                for (key, mut value) in original {
                    let redacted = self.redact(&key);
                    count += redacted.count + self.redact_json(&mut value);
                    let mut key = redacted.text;
                    // Distinct secret keys must not collapse entries into one marker.
                    while values.contains_key(&key) {
                        key.push('_');
                    }
                    values.insert(key, value);
                }
                count
            }
            _ => 0,
        }
    }
}

impl fmt::Debug for SecretRedactor {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SecretRedactor")
            .field(
                "known_values",
                &format_args!("<{} redacted>", self.known_values.len()),
            )
            .finish()
    }
}

fn collect_spans<'a>(
    text: &'a str,
    matcher: impl Fn(&'a str) -> Option<&'a str>,
    rule: SecretRule,
    spans: &mut Vec<(usize, usize, SecretRule)>,
) {
    let mut offset = 0;
    while let Some(matched) = matcher(&text[offset..]) {
        let start = offset
            + text[offset..]
                .find(matched)
                .expect("matched credential exists");
        let end = start + matched.len();
        spans.push((start, end, rule));
        offset = end;
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

fn detect_key_shape(text: &str) -> Option<(SecretRule, &str)> {
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
        let window = &rest[..rest.floor_char_boundary(rest.len().min(80))];
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
        let guard = SecretRedactor::with_known_values([KNOWN_VALUE.to_owned(), "short".to_owned()]);

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
}

#[cfg(test)]
mod redaction_tests;
