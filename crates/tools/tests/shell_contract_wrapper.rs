use tools::{CommandVerdict, ShellCommandContract};

const MERGE: &str = "gh pr merge 101 --repo o/r --squash --match-head-commit 0123456789abcdef0123456789abcdef01234567";

fn wrapped(command: &str) -> Vec<String> {
    vec!["-c".to_owned(), command.to_owned()]
}

// Given: merge contract / When: a simple sh -c wrapper / Then: allow.
#[test]
fn sh_c_wrapped_merge_pr_matches_allowlist() {
    assert_eq!(
        ShellCommandContract::merge_only().evaluate("sh", &wrapped(MERGE)),
        CommandVerdict::Allow
    );
}

// Given: merge contract / When: malformed SHA / Then: deny.
#[test]
fn sh_c_wrapped_merge_pr_with_wrong_sha_denied() {
    for sha in ["abc", "gggggggggggggggggggggggggggggggggggggggg"] {
        let command = format!("gh pr merge 101 --repo o/r --squash --match-head-commit {sha}");
        assert!(matches!(
            ShellCommandContract::merge_only().evaluate("sh", &wrapped(&command)),
            CommandVerdict::Deny { .. }
        ));
    }
}

// Given: standard contract / When: wrapped forbidden commands / Then: deny.
#[test]
fn sh_c_wrapped_denylist_still_denies() {
    for command in [
        MERGE,
        "gh issue create",
        "gh issue edit 1",
        "gh issue close 1",
        "intent-cli queue list",
        "echo intent-cli queue",
    ] {
        assert!(
            matches!(
                ShellCommandContract::standard().evaluate("sh", &wrapped(command)),
                CommandVerdict::Deny { .. }
            ),
            "{command}"
        );
    }
}

// Given: direct gh argv / When: merge contract evaluation / Then: still allow.
#[test]
fn non_sh_program_unchanged() {
    let args: Vec<String> = MERGE
        .split_whitespace()
        .skip(1)
        .map(str::to_owned)
        .collect();
    assert_eq!(
        ShellCommandContract::merge_only().evaluate("gh", &args),
        CommandVerdict::Allow
    );
}

// Given: delivery contract / When: wrapped prefix matches / Then: allow.
#[test]
fn sh_c_wrapped_delivery_prefix_matches_allowlist() {
    for command in [
        "git push origin main",
        "gh pr view 101 --repo o/r",
        "intent-cli worker complete unit",
    ] {
        assert_eq!(
            ShellCommandContract::delivery().evaluate("sh", &wrapped(command)),
            CommandVerdict::Allow
        );
    }
}

// Given: delivery contract / When: syntax needs shell parsing / Then: no unwrap.
#[test]
fn complex_shell_syntax_is_not_unwrapped() {
    for command in [
        "git push; echo bad",
        "git push | cat",
        "git push && echo bad",
        "git push\necho bad",
        "git push >out",
        "git push 'origin'",
        "git push \"origin\"",
        "git push $REMOTE",
        "git push `echo origin`",
        "git push $(echo origin)",
        "git push *",
        "git push ?",
        "git push [ab]",
        "git push ~",
        "git push #comment",
        "git push origin\\ main",
        "git push {a,b}",
        "git push\u{a0}origin",
    ] {
        assert!(
            matches!(
                ShellCommandContract::delivery().evaluate("sh", &wrapped(command)),
                CommandVerdict::Deny { .. }
            ),
            "{command:?}"
        );
    }
}

// Given: delivery contract / When: other wrapper shapes / Then: deny.
#[test]
fn other_wrappers_are_not_unwrapped() {
    for (program, args) in [
        ("bash", wrapped("git push")),
        ("/bin/sh", wrapped("git push")),
        ("sh", vec!["-lc".to_owned(), "git push".to_owned()]),
        (
            "sh",
            vec!["-c".to_owned(), "git push".to_owned(), "extra".to_owned()],
        ),
        ("sh", wrapped("")),
    ] {
        assert!(matches!(
            ShellCommandContract::delivery().evaluate(program, &args),
            CommandVerdict::Deny { .. }
        ));
    }
}
