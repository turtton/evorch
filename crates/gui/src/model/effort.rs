//! 推論強度の選択肢。routing 候補と chat の model picker で共有する。

/// モデルに effort_levels が未設定のときに提示する共通の推論強度一覧。
pub const DEFAULT_EFFORT_LEVELS: [&str; 7] =
    ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

/// モデルに設定された推論強度一覧を返す。未設定なら共通既定一覧。
pub fn effort_choices(levels: Option<&[String]>) -> Vec<String> {
    levels.map_or_else(
        || DEFAULT_EFFORT_LEVELS.map(str::to_owned).into(),
        <[String]>::to_vec,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_choices_prefers_model_levels_and_defaults_end_at_max() {
        // Given: a model without levels and a model with its own levels.
        let custom = vec!["custom-level".to_owned()];
        // When/Then: defaults end with max, while configured levels win unchanged.
        assert_eq!(effort_choices(None).last().map(String::as_str), Some("max"));
        assert_eq!(effort_choices(Some(&custom)), custom);
    }
}
