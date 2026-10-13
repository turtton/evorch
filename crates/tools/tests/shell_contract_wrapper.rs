use tools::{CommandVerdict, ShellCommandContract};

fn wrapped(command: &str) -> Vec<String> {
    vec!["-c".to_owned(), command.to_owned()]
}

// Given: standard contract / When: wrapped forbidden commands / Then: deny.
#[test]
fn sh_c_wrapped_denylist_still_denies() {
    for command in [
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
