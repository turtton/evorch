use super::*;

#[test]
fn oauth_headers_match_cli_identity_without_authentication_or_credit_opt_in() {
    let profile = ClaudeWireProfile;
    for enabled in [false, true] {
        let headers = profile.headers("session-id", enabled).unwrap();
        assert_eq!(headers["user-agent"], CLAUDE_USER_AGENT);
        assert!(CLAUDE_USER_AGENT.contains(CLAUDE_CODE_VERSION));
        assert!(CLAUDE_SYSTEM_IDENTITY.starts_with("You are Claude Code,"));
        assert_eq!(headers["x-claude-code-session-id"], "session-id");
        assert_eq!(headers["x-stainless-package-version"], "0.128.0");
        assert_eq!(headers["x-stainless-runtime-version"], "v26.3.0");
        assert_eq!(headers["accept-encoding"], "gzip, deflate, br, zstd");
        assert_eq!(headers["anthropic-dangerous-direct-browser-access"], "true");
        let betas = headers["anthropic-beta"].to_str().unwrap();
        assert_eq!(betas.contains("effort-2025-11-24"), enabled);
        assert!(!betas.contains("context-1m"));
        assert!(!betas.contains("fallback-credit"));
        assert!(!headers.contains_key("authorization"));
        assert!(!headers.contains_key("x-api-key"));
    }
    assert!(profile.headers("bad\r\nheader", false).is_err());
    for (arch, expected) in [("x86_64", "x64"), ("aarch64", "arm64"), ("x86", "x86")] {
        assert_eq!(stainless_arch(arch), expected);
    }
    for (os, expected) in [
        ("linux", "Linux"),
        ("macos", "MacOS"),
        ("windows", "Windows"),
        ("freebsd", "FreeBSD"),
        ("openbsd", "OpenBSD"),
    ] {
        assert_eq!(stainless_os(os), expected);
    }
    assert_eq!(stainless_arch("riscv64"), "other::riscv64");
    assert_eq!(stainless_os("netbsd"), "Other::netbsd");
}

#[test]
fn adaptive_models_emit_effort_and_preserve_thinking_across_tool_turns() {
    let profile = ClaudeWireProfile;
    for model in ["claude-sonnet-4-6", "claude-opus-4-6", "claude-opus-4-7"] {
        for effort in ["low", "medium", "high", "max"] {
            let controls = profile.reasoning(model, 8192, Some(effort)).unwrap();
            assert_eq!(
                controls.thinking,
                Some(json!({"type":"adaptive","display":"omitted"}))
            );
            assert_eq!(controls.output_config, Some(json!({"effort": effort})));
            assert_eq!(
                controls.context_management,
                Some(json!({"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}))
            );
            assert!(controls.is_enabled());
        }
    }
    assert!(
        profile
            .reasoning("claude-opus-4-7", 8192, Some("xhigh"))
            .is_ok()
    );
    for model in ["claude-sonnet-4-6", "claude-opus-4-6"] {
        assert!(profile.reasoning(model, 8192, Some("xhigh")).is_err());
    }
}

#[test]
fn manual_thinking_budgets_are_bounded_and_effort_is_only_sent_where_supported() {
    let profile = ClaudeWireProfile;
    for model in [
        "claude-3-7-sonnet-20250219",
        "claude-sonnet-4-20250514",
        "claude-opus-4-0",
        "claude-opus-4-1-20250805",
        "claude-sonnet-4-5",
        "claude-haiku-4-5",
        "claude-opus-4-5-20251101",
    ] {
        for (effort, budget) in [("low", 1024), ("medium", 4096), ("high", 16384)] {
            for max_tokens in [1025, 8192, 32768] {
                let controls = profile.reasoning(model, max_tokens, Some(effort)).unwrap();
                assert_eq!(
                    controls.thinking,
                    Some(json!({"type":"enabled","budget_tokens":budget.min(max_tokens-1)}))
                );
                assert_eq!(
                    controls.output_config.is_some(),
                    model.starts_with("claude-opus-4-5")
                );
                assert_eq!(
                    controls.context_management.is_some(),
                    !model.starts_with("claude-3-7")
                );
            }
        }
        assert!(profile.reasoning(model, 1024, Some("low")).is_err());
        assert!(profile.reasoning(model, 0, Some("high")).is_err());
        assert!(profile.reasoning(model, 8192, Some("max")).is_err());
    }
}

#[test]
fn unspecified_thinking_and_explicit_disable_have_distinct_wire_contracts() {
    let profile = ClaudeWireProfile;
    assert_eq!(
        serde_json::to_value(profile.reasoning("unknown", 0, None).unwrap()).unwrap(),
        json!({})
    );
    for model in ["claude-opus-4-7", "claude-sonnet-4-6", "claude-sonnet-4-5"] {
        let controls = profile.reasoning(model, 8192, Some("none")).unwrap();
        assert_eq!(
            serde_json::to_value(&controls).unwrap(),
            json!({"thinking":{"type":"disabled"}})
        );
        assert!(!controls.is_enabled());
    }
    for model in [
        "claude-opus-4-8",
        "claude-opus-4-70",
        "claude-opus-4-7-custom",
        "unknown",
    ] {
        assert!(profile.reasoning(model, 8192, Some("none")).is_err());
        assert!(profile.reasoning(model, 8192, Some("high")).is_err());
    }
    assert!(
        profile
            .reasoning("claude-opus-4-7", 8192, Some("very-high"))
            .is_err()
    );
}
