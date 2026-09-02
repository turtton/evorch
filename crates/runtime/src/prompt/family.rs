//! モデルファミリ分類 (issue #49 / AC4)。
//!
//! model id から [`ModelFamily`] を純粋に分類する。分類は fail-safe であり、
//! 未知の id は常に [`ModelFamily::Unknown`] にフォールバックする。

/// model id のファミリ分類結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelFamily {
    /// Anthropic Claude 系。
    Claude,
    /// OpenAI の reasoning 系 (o1 / o3 / o4)。
    OpenAiReasoning,
    /// OpenAI の GPT-5 系。
    Gpt5,
    /// Google Gemini 系。
    Gemini,
    /// Moonshot Kimi 系。
    Kimi,
    /// 分類不能 (fail-safe フォールバック)。
    Unknown,
}

impl ModelFamily {
    /// ファミリに対応するプリセットの base section キー。
    ///
    /// [`ModelFamily::Unknown`] は汎用プリセット `family-generic` に対応する。
    pub fn base_section_key(&self) -> &'static str {
        match self {
            ModelFamily::Claude => "family-claude",
            ModelFamily::OpenAiReasoning => "family-openai-reasoning",
            ModelFamily::Gpt5 => "family-gpt5",
            ModelFamily::Gemini => "family-gemini",
            ModelFamily::Kimi => "family-kimi",
            ModelFamily::Unknown => "family-generic",
        }
    }
}

/// model id を [`ModelFamily`] に分類する純粋関数。
///
/// 判定は小文字化した id に対して行う。どの規則にも一致しない場合は
/// [`ModelFamily::Unknown`] を返す (fail-safe)。
pub fn classify(model_id: &str) -> ModelFamily {
    let id = model_id.to_ascii_lowercase();
    if id.contains("claude") {
        ModelFamily::Claude
    } else if id.starts_with("gpt-5") {
        ModelFamily::Gpt5
    } else if id.starts_with("o1") || id.starts_with("o3") || id.starts_with("o4") {
        ModelFamily::OpenAiReasoning
    } else if id.contains("gemini") {
        ModelFamily::Gemini
    } else if id.contains("kimi") {
        ModelFamily::Kimi
    } else {
        ModelFamily::Unknown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Given: 既知の model id 群
    // When: classify する
    // Then: 対応するファミリに分類される
    #[test]
    fn classify_maps_known_model_ids_to_families() {
        let cases = [
            ("claude-opus-4-1", ModelFamily::Claude),
            ("CLAUDE-sonnet-4-5", ModelFamily::Claude),
            ("claude-3-5-haiku", ModelFamily::Claude),
            ("o1-preview", ModelFamily::OpenAiReasoning),
            ("o3-mini", ModelFamily::OpenAiReasoning),
            ("o4-mini", ModelFamily::OpenAiReasoning),
            ("gpt-5", ModelFamily::Gpt5),
            ("gpt-5-codex", ModelFamily::Gpt5),
            ("gemini-2.5-pro", ModelFamily::Gemini),
            ("GEMINI-2.0-flash", ModelFamily::Gemini),
            ("kimi-k2", ModelFamily::Kimi),
            ("kimi-latest", ModelFamily::Kimi),
        ];
        for (model_id, expected) in cases {
            assert_eq!(classify(model_id), expected, "model_id = {model_id}");
        }
    }

    // Given: 既知の規則に一致しない model id
    // When: classify する
    // Then: Unknown にフォールバックする
    #[test]
    fn classify_unknown_ids_falls_back_to_unknown() {
        let cases = ["gpt-4o", "deepseek-v3", ""];
        for model_id in cases {
            assert_eq!(
                classify(model_id),
                ModelFamily::Unknown,
                "model_id = {model_id}"
            );
        }
    }

    // Given: Unknown ファミリ
    // When: base_section_key を取得する
    // Then: 汎用プリセット family-generic に対応する
    #[test]
    fn unknown_family_maps_to_generic_base_section() {
        assert_eq!(ModelFamily::Unknown.base_section_key(), "family-generic");
    }
}
