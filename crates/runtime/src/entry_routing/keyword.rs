//! ローカルキーワードによる entry 事前ルーティング判定 (issue #71)。
//!
//! ユーザーメッセージから direct 系 / coordination 系キーワードを検出し、
//! Orchestrator 起動前の実行形態事前判定に使う初速を返す。
//! コードブロック・インラインコード・スラッシュコマンド行は分類対象から除外する。

use std::sync::OnceLock;

use regex::Regex;

/// direct 実行を明示するキーワード。
pub const DIRECT_KEYWORDS: [&str; 2] = ["direct", "just"];

/// coordination 実行を明示するキーワード。
pub const COORDINATION_KEYWORDS: [&str; 4] =
    ["orchestrator", "orchestrate", "coordinate", "delegate"];

static DIRECT_RE: OnceLock<Regex> = OnceLock::new();
static COORDINATION_RE: OnceLock<Regex> = OnceLock::new();
static FENCED_CODE_BLOCK_RE: OnceLock<Regex> = OnceLock::new();
static INLINE_CODE_RE: OnceLock<Regex> = OnceLock::new();

/// ローカルキーワードルールの判定結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalVerdict {
    /// explicit direct キーワードを高確度で検出した。
    Direct {
        /// 検出したキーワード (小文字化済み)。
        keyword: String,
    },
    /// direct キーワードなし。Orchestrator 起動のデフォルト。
    Coordinated,
    /// ローカルルールでは判定不能 (矛盾 or 分類対象テキスト無し)。
    Uncertain(UncertainReason),
}

/// 判定不能の理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UncertainReason {
    /// direct 系と coordination 系キーワードが共存した。
    Contradiction {
        /// direct 系ヒット (小文字化済み・出現順・重複なし)。
        direct: Vec<String>,
        /// coordination 系ヒット。
        coordination: Vec<String>,
    },
    /// 除外処理後に分類対象テキストが残らなかった。
    NoClassifiableText,
}

/// ユーザーが書いたゴールテキストをローカルキーワードルールで分類する。
///
/// この関数が受け取ってよいのはユーザー起点のテキストのみである。内部生成
/// メッセージやシステムメッセージは構造的に除外対象外であり、呼び出し側が
/// 渡してはならない。
pub fn classify_local(message: &str) -> LocalVerdict {
    let classifiable = strip_exclusions(message);
    if classifiable.trim().is_empty() {
        return LocalVerdict::Uncertain(UncertainReason::NoClassifiableText);
    }

    let direct = unique_lowered_hits(direct_regex(), &classifiable);
    let coordination = unique_lowered_hits(coordination_regex(), &classifiable);
    let first_direct = direct.first().cloned();

    match (first_direct, coordination.is_empty()) {
        (Some(keyword), true) => LocalVerdict::Direct { keyword },
        (Some(_), false) => LocalVerdict::Uncertain(UncertainReason::Contradiction {
            direct,
            coordination,
        }),
        (None, _) => LocalVerdict::Coordinated,
    }
}

/// キーワード配列から大文字小文字を無視する単語境界付き正規表現を組み立てる。
///
/// `\b` は Unicode 対応だがキーワードはすべて ASCII のため ASCII 語境界として
/// 機能し、"directory" や "justify" のような部分一致を弾く。
fn keyword_regex(keywords: &[&str]) -> Regex {
    let alternation = keywords
        .iter()
        .map(|keyword| regex::escape(keyword))
        .collect::<Vec<_>>()
        .join("|");
    Regex::new(&format!(r"(?i)\b(?:{alternation})\b")).expect("keyword regex is statically valid")
}

/// fenced code block 除外正規表現。終端のない fence はテキスト末尾まで除外する。
fn fenced_code_block_regex() -> &'static Regex {
    FENCED_CODE_BLOCK_RE.get_or_init(|| {
        Regex::new(r"(?s)```.*?(?:```|\z)").expect("fence regex is statically valid")
    })
}

/// inline code 除外正規表現。
fn inline_code_regex() -> &'static Regex {
    INLINE_CODE_RE
        .get_or_init(|| Regex::new(r"`[^`\n]+`").expect("inline code regex is statically valid"))
}

fn direct_regex() -> &'static Regex {
    DIRECT_RE.get_or_init(|| keyword_regex(&DIRECT_KEYWORDS))
}

fn coordination_regex() -> &'static Regex {
    COORDINATION_RE.get_or_init(|| keyword_regex(&COORDINATION_KEYWORDS))
}

/// 除外対象を指定順 (fenced code block → inline code → スラッシュコマンド行)
/// で取り除く。
fn strip_exclusions(message: &str) -> String {
    let without_fences = fenced_code_block_regex()
        .replace_all(message, "")
        .into_owned();
    let without_inline_code = inline_code_regex()
        .replace_all(&without_fences, "")
        .into_owned();
    without_inline_code
        .lines()
        .filter(|line| !line.trim_start().starts_with('/'))
        .collect::<Vec<_>>()
        .join("\n")
}

/// 正規表現のヒットを小文字化し、出現順・重複なしで集める。
fn unique_lowered_hits(re: &Regex, text: &str) -> Vec<String> {
    let mut hits: Vec<String> = Vec::new();
    for matched in re.find_iter(text) {
        let keyword = matched.as_str().to_lowercase();
        if !hits.contains(&keyword) {
            hits.push(keyword);
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: direct キーワードで始まるユーザーメッセージ
    // When: classify_local で分類する
    // Then: Direct 判定になり keyword は小文字化された "direct"
    #[test]
    fn direct_keyword_yields_direct_verdict() {
        assert_eq!(
            classify_local("direct: fix the typo in README"),
            LocalVerdict::Direct {
                keyword: "direct".to_string()
            }
        );
    }

    // Given: just キーワードを含むユーザーメッセージ
    // When: classify_local で分類する
    // Then: Direct 判定になり keyword は "just"
    #[test]
    fn just_keyword_yields_direct_verdict() {
        assert_eq!(
            classify_local("just rename the field"),
            LocalVerdict::Direct {
                keyword: "just".to_string()
            }
        );
    }

    // Given: 大文字小文字が混在したキーワードを含むメッセージ
    // When: classify_local で分類する
    // Then: いずれも Direct 判定になり keyword は小文字化される
    #[test]
    fn keywords_are_case_insensitive() {
        assert_eq!(
            classify_local("DIRECT fix"),
            LocalVerdict::Direct {
                keyword: "direct".to_string()
            }
        );
        assert_eq!(
            classify_local("Just do it"),
            LocalVerdict::Direct {
                keyword: "just".to_string()
            }
        );
    }

    // Given: キーワードが単語の一部として埋め込まれたメッセージ
    // When: classify_local で分類する
    // Then: 単語境界を満たさないためすべて Coordinated
    #[test]
    fn keyword_requires_word_boundary() {
        let messages = [
            "list the directory",
            "justify the layout",
            "adjust padding",
            "redirect stdout",
        ];

        for message in messages {
            assert_eq!(
                classify_local(message),
                LocalVerdict::Coordinated,
                "message: {}",
                message
            );
        }
    }

    // Given: fenced code block 内にキーワードを含むメッセージ
    // When: classify_local で分類する
    // Then: コードブロックは除外され Coordinated
    #[test]
    fn keyword_inside_fenced_code_block_is_ignored() {
        assert_eq!(
            classify_local("implement this:\n```\njust run direct\n```"),
            LocalVerdict::Coordinated
        );
    }

    // Given: inline code 内にキーワードを含むメッセージ
    // When: classify_local で分類する
    // Then: inline code は除外され Coordinated
    #[test]
    fn keyword_inside_inline_code_is_ignored() {
        assert_eq!(
            classify_local("rename `direct` field"),
            LocalVerdict::Coordinated
        );
    }

    // Given: スラッシュコマンド行にキーワードを含むメッセージ
    // When: classify_local で分類する
    // Then: スラッシュコマンド行は除外され Coordinated
    #[test]
    fn keyword_on_slash_command_line_is_ignored() {
        assert_eq!(
            classify_local("/direct\nimplement feature"),
            LocalVerdict::Coordinated
        );
    }

    // Given: キーワードを含まないメッセージ
    // When: classify_local で分類する
    // Then: Orchestrator 起動のデフォルトである Coordinated
    #[test]
    fn no_keyword_yields_coordinated() {
        assert_eq!(
            classify_local("implement issue #65"),
            LocalVerdict::Coordinated
        );
    }

    // Given: coordination 系キーワードのみを含むメッセージ
    // When: classify_local で分類する
    // Then: coordination 系だけでは Worker にルートしないため Coordinated
    #[test]
    fn coordination_keyword_alone_is_coordinated() {
        assert_eq!(
            classify_local("let the orchestrator plan this"),
            LocalVerdict::Coordinated
        );
    }

    // Given: direct 系と coordination 系キーワードが共存するメッセージ
    // When: classify_local で分類する
    // Then: 判定不能 (Contradiction) になりヒット一覧が小文字化・出現順・重複なしで返る
    #[test]
    fn direct_and_coordination_keywords_are_a_contradiction() {
        assert_eq!(
            classify_local("direct fix, but delegate the tests"),
            LocalVerdict::Uncertain(UncertainReason::Contradiction {
                direct: vec!["direct".to_string()],
                coordination: vec!["delegate".to_string()],
            })
        );
    }

    // Given: 除外対象のみの入力 (空白のみ・空を含む)
    // When: classify_local で分類する
    // Then: 判定不能 (NoClassifiableText)
    #[test]
    fn only_excluded_text_is_uncertain() {
        let messages = ["```\ndirect\n```", "   ", ""];

        for message in messages {
            assert_eq!(
                classify_local(message),
                LocalVerdict::Uncertain(UncertainReason::NoClassifiableText),
                "message: {}",
                message
            );
        }
    }

    // Given: 仕様で固定されたキーワード表
    // When: 定数を比較する
    // Then: 両定数とも仕様どおりの内容と順序
    #[test]
    fn keyword_table_matches_documented_sets() {
        assert_eq!(DIRECT_KEYWORDS, ["direct", "just"]);
        assert_eq!(
            COORDINATION_KEYWORDS,
            ["orchestrator", "orchestrate", "coordinate", "delegate"]
        );
    }
}
