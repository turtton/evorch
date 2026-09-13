use super::protocol::Code;
use std::{borrow::Cow, path::Path};

pub(super) fn code(code: &Code) -> serde_json::Value {
    match code {
        Code::Number(number) => serde_json::json!(number),
        // Even short alphanumeric strings may be credentials, not diagnostic identifiers.
        Code::Text(_) => serde_json::json!("[redacted]"),
    }
}

pub(super) fn file(path: &Path) -> Cow<'_, str> {
    let text = path.to_string_lossy();
    if text.chars().any(char::is_control) {
        Cow::Borrowed("[redacted]")
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::protocol::Code;

    #[test]
    fn lsp_codes_redact_all_text_when_building_detail() {
        // Given: arbitrary server strings, including alphabet-only credentials.
        for text in [
            "E0308",
            "AKIAIOSFODNN7EXAMPLE",
            "token\nvalue",
            "",
            &"x".repeat(65),
        ] {
            // When/Then: no string code is trusted as diagnostic metadata.
            assert_eq!(
                code(&Code::Text(text.into())),
                serde_json::json!("[redacted]")
            );
        }
        assert_eq!(code(&Code::Number(i32::MIN)), serde_json::json!(i32::MIN));
        assert_eq!(code(&Code::Number(i32::MAX)), serde_json::json!(i32::MAX));
    }

    #[test]
    fn lsp_path_redacts_controls_when_building_detail() {
        // Given/When/Then: known paths remain useful, controls cannot reach subscribers.
        assert_eq!(
            file(std::path::Path::new("/workspace/sample file.rs")),
            "/workspace/sample file.rs"
        );
        for path in [
            "/workspace/a\nb",
            "/workspace/a\u{7f}",
            "/workspace/a\u{85}",
        ] {
            assert_eq!(file(std::path::Path::new(path)), "[redacted]");
        }
    }
}
