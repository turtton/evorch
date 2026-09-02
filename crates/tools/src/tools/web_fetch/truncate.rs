/// モデルへ返す出力の最大バイト数。
pub(crate) const MAX_MODEL_OUTPUT_BYTES: usize = 50 * 1024;

/// 出力の切り詰め結果に関するメタデータ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TruncationInfo {
    pub truncated: bool,
    pub original_bytes: usize,
}

/// モデル向け出力を UTF-8 の文字境界で最大長まで切り詰める。
pub(crate) fn truncate_model_output(s: String) -> (String, TruncationInfo) {
    let original_bytes = s.len();
    if original_bytes <= MAX_MODEL_OUTPUT_BYTES {
        return (
            s,
            TruncationInfo {
                truncated: false,
                original_bytes,
            },
        );
    }

    let mut end = MAX_MODEL_OUTPUT_BYTES;
    while !s.is_char_boundary(end) {
        end -= 1;
    }

    (
        s[..end].to_owned(),
        TruncationInfo {
            truncated: true,
            original_bytes,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_noop_below_limit() {
        let input = "x".repeat(MAX_MODEL_OUTPUT_BYTES - 1);

        let (output, info) = truncate_model_output(input.clone());

        assert_eq!(output, input);
        assert_eq!(
            info,
            TruncationInfo {
                truncated: false,
                original_bytes: MAX_MODEL_OUTPUT_BYTES - 1,
            }
        );
    }

    #[test]
    fn truncate_exact_limit_untouched() {
        let input = "x".repeat(MAX_MODEL_OUTPUT_BYTES);

        let (output, info) = truncate_model_output(input.clone());

        assert_eq!(output, input);
        assert_eq!(
            info,
            TruncationInfo {
                truncated: false,
                original_bytes: MAX_MODEL_OUTPUT_BYTES,
            }
        );
    }

    #[test]
    fn truncate_respects_utf8_boundary() {
        let input = format!("{}€tail", "x".repeat(MAX_MODEL_OUTPUT_BYTES - 1));

        let (output, info) = truncate_model_output(input);

        assert!(std::str::from_utf8(output.as_bytes()).is_ok());
        assert!(output.len() < MAX_MODEL_OUTPUT_BYTES);
        assert!(info.truncated);
    }

    #[test]
    fn truncate_reports_original_bytes() {
        let input = "x".repeat(MAX_MODEL_OUTPUT_BYTES + 1);
        let original_bytes = input.len();

        let (_, info) = truncate_model_output(input);

        assert_eq!(info.original_bytes, original_bytes);
        assert!(info.truncated);
    }
}
