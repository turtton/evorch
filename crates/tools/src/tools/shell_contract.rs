//! shell コマンド契約（S9）。
//!
//! モデル起因の shell 実行からは `gh issue create|edit|close` と intent-cli の
//! queue/automation/issue/packet/publish/run 系を拒否する。
//! `gh pr merge` は通常の sandbox / escalation review 経路で実行する。
//! 拒否判定は [`crate::tools::shell::Shell`] が `Sandbox::wrap` の前に行う。

use std::sync::OnceLock;

use regex::Regex;

/// コマンド評価の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandVerdict {
    /// 実行を許可する。
    Allow,
    /// 実行を拒否する。`reason` はモデルへ提示される説明。
    Deny { reason: String },
}

/// shell コマンドの実行可否を判定する契約。
#[derive(Debug, Clone)]
pub struct ShellCommandContract;

impl ShellCommandContract {
    /// モデル起因の shell に適用する deny-list 契約。
    ///
    /// `gh issue create|edit|close`、intent-cli の
    /// queue/automation/issue/packet/publish/run を拒否し、それ以外
    /// （`git add/commit`、`cargo` など）は許可する。
    pub fn standard() -> Self {
        Self
    }

    /// コマンドラインを評価して実行可否を返す。
    pub fn evaluate(&self, program: &str, args: &[String]) -> CommandVerdict {
        // Preserve interpreter deny-list over-matching before unwrapping.
        if let verdict @ CommandVerdict::Deny { .. } = evaluate_standard(program, args) {
            return verdict;
        }
        if program == "sh"
            && let [flag, command] = args
            && flag == "-c"
            // Only literal words separated by shell whitespace; no syntax or expansion.
            && command.bytes().all(|byte| {
                byte.is_ascii_alphanumeric()
                    || matches!(byte, b' ' | b'\t' | b'_' | b'-' | b'.' | b'/' | b':' | b',' | b'=' | b'%' | b'+' | b'@')
            })
        {
            let mut tokens = command.split_whitespace();
            if let Some(inner_program) = tokens.next() {
                let inner_args: Vec<String> = tokens.map(str::to_owned).collect();
                return self.evaluate(inner_program, &inner_args);
            }
        }
        CommandVerdict::Allow
    }
}

/// standard 契約の評価。deny-list に該当した場合のみ拒否する。
fn evaluate_standard(program: &str, args: &[String]) -> CommandVerdict {
    let base = basename(program);
    if base == "gh"
        && args.first().map(String::as_str) == Some("issue")
        && matches!(
            args.get(1).map(String::as_str),
            Some("create" | "edit" | "close")
        )
    {
        return CommandVerdict::Deny {
            reason: format!(
                "gh issue {} is not allowed from model-invoked shells",
                args[1]
            ),
        };
    }
    if base == "intent-cli"
        && let Some(sub) = args.first().map(String::as_str)
        && matches!(
            sub,
            "queue" | "automation" | "issue" | "packet" | "publish" | "run"
        )
    {
        return CommandVerdict::Deny {
            reason: format!("intent-cli {sub} is not allowed from model-invoked shells"),
        };
    }
    if matches!(base, "sh" | "bash" | "zsh" | "dash")
        && let Some(denied) = interpreter_deny_reason(args)
    {
        return CommandVerdict::Deny {
            reason: format!("interpreter argument contains a denied command: {denied}"),
        };
    }
    CommandVerdict::Allow
}

/// インタプリタへ渡される文字列中の拒否対象コマンドを best-effort で検出する。
///
/// インタプリタ引数は完全には解析しないため、計画 S9 通りの正規表現一致のみ
/// 行う。
fn interpreter_deny_reason(args: &[String]) -> Option<String> {
    static INTENT_CLI_FAMILY: OnceLock<Regex> = OnceLock::new();
    let intent_cli_family = INTENT_CLI_FAMILY.get_or_init(|| {
        Regex::new(r"\bintent-cli\s+(queue|automation|issue|packet|publish|run)\b")
            .expect("正規表現は有効であるはずです")
    });
    for arg in args {
        if let Some(captures) = intent_cli_family.captures(arg) {
            return Some(format!("intent-cli {}", &captures[1]));
        }
    }
    None
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_string()).collect()
    }

    // Given: パス付きとパス無しのプログラム名 / When: basename を計算 / Then: 最後のパス要素が返る
    #[test]
    fn basename_extracts_last_path_component() {
        assert_eq!(basename("gh"), "gh");
        assert_eq!(basename("/usr/bin/gh"), "gh");
        assert_eq!(basename("sh"), "sh");
    }

    // Given: standard 契約 / When: gh issue create|edit|close を評価 / Then: 拒否される
    #[test]
    fn standard_denies_gh_issue_create_edit_close() {
        let contract = ShellCommandContract::standard();
        for action in ["create", "edit", "close"] {
            assert_denied(&contract, "gh", &argv(&["issue", action, "--repo", "o/r"]));
        }
        // list / view は許可される
        assert_allowed(&contract, "gh", &argv(&["issue", "list"]));
        assert_allowed(&contract, "gh", &argv(&["issue", "view", "1"]));
    }

    // Given: standard 契約 / When: zsh / dash 経由で拒否対象を渡す / Then: 拒否される
    #[test]
    fn standard_denies_via_zsh_and_dash_interpreters() {
        let contract = ShellCommandContract::standard();
        for interpreter in ["zsh", "dash"] {
            assert_denied(
                &contract,
                interpreter,
                &argv(&["-c", "intent-cli queue seed"]),
            );
        }
    }

    // Given: standard 契約 / When: 拒否対象を含まないインタプリタ文字列を渡す / Then: 許可される
    #[test]
    fn standard_allows_benign_interpreter_strings() {
        let contract = ShellCommandContract::standard();
        assert_allowed(
            &contract,
            "sh",
            &argv(&["-c", "grep pr merge CHANGELOG.md"]),
        );
        assert_allowed(&contract, "sh", &argv(&["-c", "echo hello world"]));
    }

    // Given: standard 契約 / When: best-effort 正規表現に一致する無害な文字列を渡す / Then: 誤検知として拒否される（仕様通りの over-match）
    #[test]
    fn standard_over_matches_interpreter_strings_by_design() {
        let contract = ShellCommandContract::standard();
        assert_denied(&contract, "sh", &argv(&["-c", "echo intent-cli queue"]));
    }

    // Given: 許可されるべき評価 / When: 補助検査を通す / Then: Allow である
    fn assert_allowed(contract: &ShellCommandContract, program: &str, args: &[String]) {
        assert_eq!(contract.evaluate(program, args), CommandVerdict::Allow);
    }

    // Given: 拒否されるべき評価 / When: 補助検査を通す / Then: 空でない reason 付きの Deny である
    fn assert_denied(contract: &ShellCommandContract, program: &str, args: &[String]) {
        match contract.evaluate(program, args) {
            CommandVerdict::Allow => {
                panic!("拒否されるべきコマンドが許可されました: {program} {args:?}")
            }
            CommandVerdict::Deny { reason } => assert!(!reason.is_empty()),
        }
    }
}
