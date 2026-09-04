//! shell コマンド契約（S9）。
//!
//! モデル起因の shell 実行からは `gh pr merge` と intent-cli の
//! queue/automation/issue/packet/publish/run 系を拒否し、supervisor の配信
//! アダプタ向けに delivery / merge_only の allowlist モードを提供する。
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

/// 契約モードが許すコマンド形状。
#[derive(Debug, Clone)]
enum Pattern {
    /// `program` が一致し、引数が `tokens` で前方一致する。
    Prefix {
        program: String,
        tokens: Vec<String>,
    },
    /// `gh pr merge <digits> --repo <owner/repo> --squash --match-head-commit
    /// <40-hex>` の位置引数込みの完全一致。
    MergePr,
}

impl Pattern {
    fn prefix(program: &str, tokens: &[&str]) -> Self {
        Pattern::Prefix {
            program: program.to_string(),
            tokens: tokens.iter().map(|token| (*token).to_string()).collect(),
        }
    }
}

#[derive(Debug, Clone)]
enum Mode {
    /// deny-list。モデル起因の全 shell に適用する。
    Standard,
    /// allow-list。列挙した形状以外は一律で拒否する。
    Allowlist(Vec<Pattern>),
}

/// shell コマンドの実行可否を判定する契約。
#[derive(Debug, Clone)]
pub struct ShellCommandContract {
    mode: Mode,
}

impl ShellCommandContract {
    /// モデル起因の shell に適用する deny-list 契約。
    ///
    /// `gh pr merge`、`gh issue create|edit|close`、intent-cli の
    /// queue/automation/issue/packet/publish/run を拒否し、それ以外
    /// （`git add/commit`、`cargo` など）は許可する。
    pub fn standard() -> Self {
        Self {
            mode: Mode::Standard,
        }
    }

    /// 配信アダプタ用の allowlist 契約。
    ///
    /// `git {push,rev-parse,status,log,fetch,ls-remote}`、
    /// `gh {auth status, pr list/view/checks/create/edit/comment}`、
    /// `intent-cli {--help, worker --help/claim/result-summary/complete}` のみ
    /// を許可する。プログラムは `git`/`gh`/`intent-cli` のいずれかの完全一致で
    /// なければならず、インタプリタ経由は一律で拒否する。
    pub fn delivery() -> Self {
        let mut patterns = Vec::new();
        for sub in ["push", "rev-parse", "status", "log", "fetch", "ls-remote"] {
            patterns.push(Pattern::prefix("git", &[sub]));
        }
        patterns.push(Pattern::prefix("gh", &["auth", "status"]));
        for sub in ["list", "view", "checks", "create", "edit", "comment"] {
            patterns.push(Pattern::prefix("gh", &["pr", sub]));
        }
        patterns.push(Pattern::prefix("intent-cli", &["--help"]));
        patterns.push(Pattern::prefix("intent-cli", &["worker", "--help"]));
        for sub in ["claim", "result-summary", "complete"] {
            patterns.push(Pattern::prefix("intent-cli", &["worker", sub]));
        }
        Self {
            mode: Mode::Allowlist(patterns),
        }
    }

    /// merge 専用の allowlist 契約。
    ///
    /// `gh pr merge <digits> --repo <owner/repo> --squash --match-head-commit
    /// <40-hex>`（位置引数の順序を含む）のみを許可する。
    pub fn merge_only() -> Self {
        Self {
            mode: Mode::Allowlist(vec![Pattern::MergePr]),
        }
    }

    /// コマンドラインを評価して実行可否を返す。
    pub fn evaluate(&self, program: &str, args: &[String]) -> CommandVerdict {
        match &self.mode {
            Mode::Standard => evaluate_standard(program, args),
            Mode::Allowlist(patterns) => {
                if patterns
                    .iter()
                    .any(|pattern| pattern_matches(pattern, program, args))
                {
                    CommandVerdict::Allow
                } else {
                    CommandVerdict::Deny {
                        reason: format!(
                            "'{}' is not in the contract allowlist",
                            render_command(program, args)
                        ),
                    }
                }
            }
        }
    }
}

fn pattern_matches(pattern: &Pattern, program: &str, args: &[String]) -> bool {
    match pattern {
        Pattern::Prefix {
            program: allowed,
            tokens,
        } => {
            program == allowed
                && args.len() >= tokens.len()
                && args.iter().zip(tokens).all(|(arg, token)| arg == token)
        }
        Pattern::MergePr => matches_merge_pr(program, args),
    }
}

/// merge_only 契約の完全一致形状。位置引数の順序まで強制する。
fn matches_merge_pr(program: &str, args: &[String]) -> bool {
    program == "gh"
        && args.len() == 8
        && args[0] == "pr"
        && args[1] == "merge"
        && is_ascii_digits(&args[2])
        && args[3] == "--repo"
        && is_owner_repo(&args[4])
        && args[5] == "--squash"
        && args[6] == "--match-head-commit"
        && is_40_hex(&args[7])
}

fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn is_owner_repo(value: &str) -> bool {
    match value.split_once('/') {
        Some((owner, repo)) => is_repo_name(owner) && is_repo_name(repo),
        None => false,
    }
}

fn is_repo_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

fn is_40_hex(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// standard 契約の評価。deny-list に該当した場合のみ拒否する。
fn evaluate_standard(program: &str, args: &[String]) -> CommandVerdict {
    let base = basename(program);
    if base == "gh" {
        if starts_with(args, &["pr", "merge"]) {
            return CommandVerdict::Deny {
                reason: "gh pr merge is not allowed from model-invoked shells; pull request \
                         merges go through the supervisor delivery adapter"
                    .to_string(),
            };
        }
        if args.first().map(String::as_str) == Some("issue")
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
    }
    if base == "intent-cli"
        && let Some(sub) = args.first().map(String::as_str)
        && matches!(
            sub,
            "queue" | "automation" | "issue" | "packet" | "publish" | "run"
        )
    {
        return CommandVerdict::Deny {
            reason: format!(
                "intent-cli {sub} is not allowed from model-invoked shells; queue and \
                 publish operations go through the supervisor delivery adapter"
            ),
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
    static REGEXES: OnceLock<(Regex, Regex)> = OnceLock::new();
    let (gh_pr_merge, intent_cli_family) = REGEXES.get_or_init(|| {
        (
            Regex::new(r"\bgh\s+pr\s+merge\b").expect("正規表現は有効であるはずです"),
            Regex::new(r"\bintent-cli\s+(queue|automation|issue|packet|publish|run)\b")
                .expect("正規表現は有効であるはずです"),
        )
    });
    for arg in args {
        if gh_pr_merge.is_match(arg) {
            return Some("gh pr merge".to_string());
        }
        if let Some(captures) = intent_cli_family.captures(arg) {
            return Some(format!("intent-cli {}", &captures[1]));
        }
    }
    None
}

fn basename(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

fn starts_with(args: &[String], prefix: &[&str]) -> bool {
    args.len() >= prefix.len() && args.iter().zip(prefix).all(|(arg, token)| arg == token)
}

fn render_command(program: &str, args: &[String]) -> String {
    let mut rendered = String::from(program);
    for arg in args {
        rendered.push(' ');
        rendered.push_str(arg);
    }
    rendered
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
        assert_denied(&contract, "zsh", &argv(&["-c", "gh pr merge 1"]));
        assert_denied(&contract, "dash", &argv(&["-c", "intent-cli queue seed"]));
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

    // Given: delivery 契約 / When: gh pr merge を評価 / Then: 拒否される
    #[test]
    fn delivery_denies_gh_pr_merge() {
        let contract = ShellCommandContract::delivery();
        assert_denied(
            &contract,
            "gh",
            &argv(&["pr", "merge", "123", "--repo", "o/r", "--squash"]),
        );
    }

    // Given: delivery 契約 / When: インタプリタ経由で許可対象を渡す / Then: プログラム完全一致のため拒否される
    #[test]
    fn delivery_denies_interpreter_programs() {
        let contract = ShellCommandContract::delivery();
        assert_denied(&contract, "sh", &argv(&["-c", "git push origin main"]));
        assert_denied(&contract, "bash", &argv(&["-lc", "gh auth status"]));
        // 許可対象の前方一致でも tokens と完全一致しない先頭引数は拒否される
        assert_denied(&contract, "git", &argv(&["pushes"]));
    }

    // Given: merge_only 契約 / When: --repo 値にドット区切り名を渡す / Then: owner/repo 形状として許可される
    #[test]
    fn merge_only_accepts_dotted_repo_names() {
        let contract = ShellCommandContract::merge_only();
        assert_allowed(
            &contract,
            "gh",
            &argv(&[
                "pr",
                "merge",
                "42",
                "--repo",
                "my-org/my.repo_1",
                "--squash",
                "--match-head-commit",
                "0123456789abcdef0123456789abcdef01234567",
            ]),
        );
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
