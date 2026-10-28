//! English display of runtime failure reasons.
//!
//! Runtime error `Display` strings are Japanese and are also what the model
//! reads (child failure reasons, tool results), so they stay as they are. The
//! GUI shows them through [`english`], keeping the original text in a hover.

/// Japanese template (`{}` captures a variable part) → English template
/// (`{0}`, `{1}`… refer to the captures). Longer, more specific templates come
/// first; a capture is itself translated, so nested reasons stay readable.
const RULES: &[(&str, &str)] = &[
    // RuntimeError
    (
        "実行 {} の復元に失敗しました: {}",
        "Could not restore run {0}: {1}",
    ),
    ("未知の AgentRun が指定されました: {}", "Unknown run: {0}"),
    (
        "ロール '{}' のツール '{}' が拒否されました: {}",
        "Role '{0}' was denied tool '{1}': {2}",
    ),
    ("不正な位相遷移です: {}", "Invalid state transition: {0}"),
    ("実行 {} は既に終了しています", "Run {0} has already ended"),
    (
        "モデル {} へ切り替える前に履歴を整えます",
        "Preparing the history before switching to model {0}",
    ),
    ("モデル呼び出しに失敗しました: {}", "Model call failed: {0}"),
    (
        "{} の {}/{} は {} のモデルのため送信しませんでした。会話と要約には chat のモデルだけを使えます",
        "{0}: {1}/{2} is a {3} model and was not used; only chat models can serve conversations and summaries",
    ),
    (
        "サンドボックス構築に失敗しました: {}",
        "Sandbox setup failed: {0}",
    ),
    (
        "workspace の初期化に失敗しました: {}",
        "Workspace setup failed: {0}",
    ),
    (
        "web ツールのネットワークガード初期化に失敗しました: {}",
        "Web tool network guard setup failed: {0}",
    ),
    (
        "AgentRun {} から {} へのメッセージが拒否されました: {}",
        "Message from run {0} to {1} was denied: {2}",
    ),
    ("未知のメッセージ ID です: {}", "Unknown message ID: {0}"),
    (
        "返信待ちがタイムアウトしました: {}",
        "Timed out waiting for a reply: {0}",
    ),
    (
        "AgentRun {} の mailbox が一杯です",
        "The mailbox of run {0} is full",
    ),
    (
        "AgentRun {} で compaction が実行中です",
        "Run {0} is compacting",
    ),
    // RunRestoreFailure
    (
        "run storage が設定されていません",
        "Run storage is not configured",
    ),
    (
        "復元用コンテキストの消費を保存できません: {}",
        "Could not record the restore context as used: {0}",
    ),
    (
        "復元用コンテキストがありません",
        "There is no restore context",
    ),
    (
        "復元用コンテキストが破損しています: {}",
        "The restore context is corrupt: {0}",
    ),
    (
        "復元できない実行設定です: {}",
        "These run settings cannot be restored: {0}",
    ),
    (
        "ユーザーが停止した実行のため継続しません。作業がまだ必要なら新しい run に委譲するか、ユーザーに確認してください",
        "The run was stopped by the user and cannot continue",
    ),
    (
        "取り消された実行のため継続しません。作業がまだ必要なら新しい run に委譲してください",
        "The run was cancelled and cannot continue",
    ),
    // restore.rs
    (
        "復元コンテキストを直列化できません: {}",
        "Could not serialize the restore context: {0}",
    ),
    (
        "復元コンテキストを保存できません: {}",
        "Could not save the restore context: {0}",
    ),
    (
        "保存時刻を取得できません: {}",
        "Could not read the save time: {0}",
    ),
    (
        "保存時刻が範囲外です: {}",
        "The save time is out of range: {0}",
    ),
    (
        "復元対象外の実行状態: {}",
        "Run state that cannot be restored: {0}",
    ),
    // message delivery
    (
        "自己宛てのメッセージは許可されていません",
        "A run cannot message itself",
    ),
    (
        "steering は親から子へのみ許可されています",
        "Steering is allowed only from a parent to its child",
    ),
    (
        "親子関係のない run 間のメッセージは許可されていません",
        "Messages between unrelated runs are not allowed",
    ),
    ("Reply には reply_to が必要です", "A reply needs reply_to"),
    // skills
    (
        "skill '{}' は登録されていません",
        "Skill '{0}' is not registered",
    ),
    (
        "skill '{}' の SKILL.md を読み込めません",
        "Could not read SKILL.md of skill '{0}'",
    ),
    (
        "skill '{}' の SKILL.md は frontmatter 構造が不正です",
        "SKILL.md of skill '{0}' has an invalid frontmatter",
    ),
    (
        "SKILL.md は先頭行が frontmatter フェンス `---` で始まる必要があります",
        "SKILL.md must start with the frontmatter fence `---`",
    ),
    (
        "SKILL.md の frontmatter に閉じフェンス行 `---` がありません",
        "The SKILL.md frontmatter has no closing `---` line",
    ),
    (
        "SKILL.md の frontmatter が不正な YAML です: {}",
        "The SKILL.md frontmatter is invalid YAML: {0}",
    ),
    (
        "SKILL.md の frontmatter に未知のフィールド '{}' があります",
        "The SKILL.md frontmatter has an unknown field '{0}'",
    ),
    (
        "SKILL.md の frontmatter に必須フィールド '{}' がありません",
        "The SKILL.md frontmatter lacks the required field '{0}'",
    ),
    (
        "SKILL.md の frontmatter フィールド 'name' は空でない文字列である必要があります",
        "The SKILL.md field 'name' must be a non-empty string",
    ),
    (
        "SKILL.md の frontmatter フィールド 'name' が 64 文字を超えています",
        "The SKILL.md field 'name' exceeds 64 characters",
    ),
    (
        "SKILL.md の frontmatter フィールド 'name' は ^[a-z0-9]+(-[a-z0-9]+)*$ に一致する必要があります",
        "The SKILL.md field 'name' must match ^[a-z0-9]+(-[a-z0-9]+)*$",
    ),
    (
        "SKILL.md の frontmatter フィールド 'name' がディレクトリ名 '{}' と一致しません",
        "The SKILL.md field 'name' does not match the directory name '{0}'",
    ),
    (
        "SKILL.md の frontmatter フィールド 'description' は空でない文字列である必要があります",
        "The SKILL.md field 'description' must be a non-empty string",
    ),
    (
        "SKILL.md の frontmatter フィールド 'description' が 1024 文字を超えています",
        "The SKILL.md field 'description' exceeds 1024 characters",
    ),
    (
        "SKILL.md の frontmatter フィールド 'compatibility' が 500 文字を超えています",
        "The SKILL.md field 'compatibility' exceeds 500 characters",
    ),
    (
        "SKILL.md の frontmatter フィールド 'allowed-tools' は空白区切りの単一文字列である必要があります",
        "The SKILL.md field 'allowed-tools' must be a single space-separated string",
    ),
    (
        "SKILL.md の frontmatter フィールド 'metadata' は文字列キーと文字列値のマップである必要があります",
        "The SKILL.md field 'metadata' must map string keys to string values",
    ),
    (
        "SKILL.md の frontmatter フィールド '{}' は文字列である必要があります",
        "The SKILL.md field '{0}' must be a string",
    ),
    ("リファレンスが不正です: {}", "Invalid reference: {0}"),
    (
        "リファレンス '{}' に対応するファイルがありません",
        "No file for reference '{0}'",
    ),
    (
        "リファレンス '{}' の内容は UTF-8 である必要があります",
        "Reference '{0}' must be UTF-8",
    ),
    (
        "リファレンス '{}' は skill ディレクトリの外を指します",
        "Reference '{0}' points outside the skill directory",
    ),
    // provider profile validation and token storage (routing)
    (
        "プロファイル名 '{}' が重複しています",
        "Duplicate profile name '{0}'",
    ),
    (
        "保存済みトークン一式の解析に失敗しました: {}",
        "Could not parse the stored token bundle: {0}",
    ),
    (
        "トークン一式の保存用 JSON への変換に失敗しました: {}",
        "Could not serialize the token bundle: {0}",
    ),
    (
        "codex client の構築に失敗しました: {}",
        "Could not build the codex client: {0}",
    ),
    (
        "openai client の構築に失敗しました: {}",
        "Could not build the openai client: {0}",
    ),
    (
        "openai-compatible client の構築に失敗しました: {}",
        "Could not build the openai-compatible client: {0}",
    ),
    (
        "credential store 操作に失敗しました: {}",
        "Credential store operation failed: {0}",
    ),
    (
        "provider type `{}` は api protocol `{}` のみをサポートします (actual: {})",
        "provider type `{0}` supports only api protocol `{1}` (actual: {2})",
    ),
    (
        "provider type `{}` は api protocol `{}` のみをサポートします (actual: {})。api_protocol を `{}` に変更してください",
        "provider type `{0}` supports only api protocol `{1}` (actual: {2}); set api_protocol to `{3}`",
    ),
    (
        "provider type `openai-codex` は keyring 認証情報参照のみをサポートします。環境変数参照ではなく keyring 参照 (service = \"evorch\", account = \"...\") に変更してください",
        "provider type `openai-codex` supports only keyring credentials; use a keyring reference (service = \"evorch\", account = \"...\") instead of an environment variable",
    ),
    // providers
    (
        "OpenAI stream chunk の解析に失敗しました: {}",
        "Could not parse an OpenAI stream chunk: {0}",
    ),
    (
        "OpenAI response の解析に失敗しました: {}",
        "Could not parse the OpenAI response: {0}",
    ),
    (
        "Responses event の解析に失敗しました: {}",
        "Could not parse a Responses event: {0}",
    ),
    (
        "device authorization interval の解析に失敗しました: {}",
        "Could not parse the device authorization interval: {0}",
    ),
    (
        "id_token payload の解析に失敗しました: {}",
        "Could not parse the id_token payload: {0}",
    ),
    (
        "Codex compaction SSE の解析に失敗しました",
        "Could not parse the Codex compaction SSE",
    ),
    (
        "Codex compaction item が重複しています",
        "Duplicate Codex compaction item",
    ),
];

const MAX_DEPTH: usize = 4;

/// Whether `text` still contains Japanese (kana, kanji or full-width forms).
pub fn contains_japanese(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c,
            '\u{3040}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}' | '\u{3000}'..='\u{303f}')
    })
}

/// `reason` with the Japanese runtime wording translated; text it does not
/// know, including English text, comes back unchanged.
pub fn english(reason: &str) -> String {
    translate(reason, MAX_DEPTH)
}

fn translate(text: &str, depth: usize) -> String {
    if depth == 0 || !contains_japanese(text) {
        return text.to_owned();
    }
    for (japanese, english) in RULES {
        let Some(captures) = capture(japanese, text) else {
            continue;
        };
        let captures: Vec<String> = captures
            .into_iter()
            .map(|part| translate(part, depth - 1))
            .collect();
        let mut out = (*english).to_owned();
        for (index, part) in captures.iter().enumerate() {
            out = out.replace(&format!("{{{index}}}"), part);
        }
        return out;
    }
    text.to_owned()
}

/// The parts of `text` the `{}` holes of `template` stand for, if it matches.
fn capture<'a>(template: &str, text: &'a str) -> Option<Vec<&'a str>> {
    let mut literals = template.split("{}");
    let mut rest = text.strip_prefix(literals.next()?)?;
    let literals: Vec<&str> = literals.collect();
    let mut parts = Vec::with_capacity(literals.len());
    for (index, literal) in literals.iter().enumerate() {
        let last = index + 1 == literals.len();
        let end = if last {
            if literal.is_empty() {
                rest.len()
            } else {
                rest.strip_suffix(literal)?.len()
            }
        } else {
            rest.find(literal)?
        };
        parts.push(&rest[..end]);
        rest = &rest[end + literal.len()..];
    }
    Some(parts)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: the Japanese Display of the typed runtime errors the model also reads
    // When: shown to the user / Then: English, with the variable parts kept
    #[test]
    fn runtime_error_displays_read_in_english() {
        use runtime::RuntimeError;
        let model = RuntimeError::Model {
            reason: "HTTP 500".into(),
        };
        assert_eq!(model.to_string(), "モデル呼び出しに失敗しました: HTTP 500");
        assert_eq!(english(&model.to_string()), "Model call failed: HTTP 500");

        let nested = RuntimeError::Model {
            reason: RuntimeError::Sandbox {
                detail: "bwrap missing".into(),
            }
            .to_string(),
        };
        assert_eq!(
            english(&nested.to_string()),
            "Model call failed: Sandbox setup failed: bwrap missing"
        );
    }

    // Given: every Japanese template / When: matched against its own wording
    // Then: nothing Japanese remains, so a template cannot silently stop matching
    #[test]
    fn every_template_translates_its_own_wording() {
        for (japanese, _) in RULES {
            let sample = japanese.replace("{}", "X");
            let shown = english(&sample);
            assert!(!contains_japanese(&shown), "{japanese} -> {shown}");
            assert!(
                !shown.contains('{'),
                "unfilled placeholder for {japanese}: {shown}"
            );
        }
    }

    // Given: English or unknown text / When: translated / Then: it is unchanged
    #[test]
    fn other_text_is_unchanged() {
        assert_eq!(english("connection lost"), "connection lost");
        assert_eq!(english("未知の失敗"), "未知の失敗");
    }
}
