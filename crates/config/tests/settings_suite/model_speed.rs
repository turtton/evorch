use config::types::provider::{FAST_MODEL_SUFFIX, ModelSpeed, fast_variant_id, parse_model_speed};

#[test]
fn parses_one_trailing_fast_marker() {
    // Given: suffix付きID / When: 解析 / Then: 末尾の一つだけを除く。
    assert_eq!(
        parse_model_speed("gpt-5.6+fast"),
        ("gpt-5.6", ModelSpeed::Fast)
    );
    assert_eq!(
        parse_model_speed("gpt+fast+fast"),
        ("gpt+fast", ModelSpeed::Fast)
    );
}

#[test]
fn standard_and_empty_base_remain_unchanged() {
    // Given: 通常・空base・末尾以外のmarker / When: 解析 / Then: Standardのまま。
    for id in ["gpt-5.6", "+fast", "", "gpt+fast-extra"] {
        assert_eq!(parse_model_speed(id), (id, ModelSpeed::Standard));
    }
}

#[test]
fn fast_variant_round_trips() {
    // Given: base ID / When: variant生成 / Then: 同じbaseとFastに復元。
    assert_eq!(FAST_MODEL_SUFFIX, "+fast");
    let variant = fast_variant_id("gpt-5.6");
    assert_eq!(variant, "gpt-5.6+fast");
    assert_eq!(parse_model_speed(&variant), ("gpt-5.6", ModelSpeed::Fast));
}
