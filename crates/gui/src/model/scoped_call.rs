//! 承認要求の scoped call ID を共通の規則で解釈する。

/// scoped ID から run ID、元の call ID、任意の attempt を取り出す。
///
/// `run-` に続く非空の ASCII 数字と `:` を必須とする。
/// 既存の run 解決との互換性のため空の call ID も受理し、
/// 第3セグメントが u64 として解釈できない場合は attempt だけを `None` にする。
/// 第4セグメント以降は解釈しない。
pub fn parse_scoped_call_id(call_id: &str) -> Option<(String, String, Option<u64>)> {
    let (run_id, remainder) = call_id.split_once(':')?;
    let number = run_id.strip_prefix("run-")?;
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let mut segments = remainder.split(':');
    let original_call_id = segments.next()?;
    let attempt = segments.next().and_then(|value| value.parse::<u64>().ok());
    Some((run_id.to_owned(), original_call_id.to_owned(), attempt))
}

#[cfg(test)]
mod tests {
    #[test]
    fn parses_scoped_call_when_run_prefix_is_valid() {
        // Given: ASCII 数字の run ID と任意の attempt。
        for (input, run, call, attempt) in [
            ("run-2:call-1:17", "run-2", "call-1", Some(17)),
            ("run-2:call-1", "run-2", "call-1", None),
            ("run-0:call:0", "run-0", "call", Some(0)),
            ("run-002:call:+2", "run-002", "call", Some(2)),
            (
                "run-2:call:18446744073709551615",
                "run-2",
                "call",
                Some(u64::MAX),
            ),
            (
                "run-18446744073709551616:call",
                "run-18446744073709551616",
                "call",
                None,
            ),
            ("run-2:", "run-2", "", None),
            ("run-2::17", "run-2", "", Some(17)),
            ("run-2:呼出:17:extra", "run-2", "呼出", Some(17)),
        ] {
            // When: scoped ID を解釈する。
            let parsed = super::parse_scoped_call_id(input);
            // Then: run と call を保持し、第3セグメントを attempt とする。
            assert_eq!(parsed, Some((run.into(), call.into(), attempt)), "{input}");
        }
    }

    #[test]
    fn rejects_scoped_call_when_run_prefix_is_malformed() {
        // Given: scoped run を確定できない ID。
        for input in [
            "",
            "call-1",
            "run-:",
            "run-x:",
            "run-2x:",
            "run-+2:",
            "run-:call",
            "run-x:call",
            "run-2x:call",
            "run-+2:call",
            "run-２:call",
            "other:call",
            "run-2",
            ":run-2:call",
            "run-2 :call",
        ] {
            // When: scoped ID を解釈する。
            let parsed = super::parse_scoped_call_id(input);
            // Then: run を推測しない。
            assert_eq!(parsed, None, "{input}");
        }
    }

    #[test]
    fn preserves_scoped_call_when_attempt_cannot_be_parsed() {
        // Given: run scope は有効だが attempt が u64 でない ID。
        for suffix in ["", "invalid", "-1", "１７", " 17", "18446744073709551616"] {
            let input = format!("run-2:call-1:{suffix}");
            // When: scoped ID を解釈する。
            let parsed = super::parse_scoped_call_id(&input);
            // Then: attempt の失敗で既存の run 解決を拒否しない。
            assert_eq!(
                parsed,
                Some(("run-2".into(), "call-1".into(), None)),
                "{input}"
            );
        }
    }

    #[test]
    fn registry_preserves_scoped_call_when_suffix_is_unrestricted() {
        // Given: 既存実装が接頭辞だけで受理していた ID。
        let registry = crate::model::transcript_registry::TranscriptRegistry::new();
        for input in [
            "run-2:",
            "run-2::17",
            "run-2:call:invalid",
            "run-2:call:17:extra",
        ] {
            // When: run の配送先を解決する。
            let run = registry.run_for_call(input);
            // Then: suffix の内容によらず同じ run を返す。
            assert_eq!(run, Some("run-2"), "{input}");
        }
    }
}
