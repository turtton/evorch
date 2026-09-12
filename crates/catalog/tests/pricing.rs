use catalog::ModelMetadata;

#[test]
fn captures_cache_prices_when_present_in_models_dev_cost() {
    // Given
    let json = r#"{"id":"y","cost":{"input":2,"output":8,"cache_read":0.2,"cache_write":3}}"#;
    // When
    let model: ModelMetadata = serde_json::from_str(json).expect("metadata parses");
    // Then
    assert_eq!(
        (model.cache_read_price, model.cache_write_price),
        (Some(0.2), Some(3.0))
    );
}

#[test]
fn missing_cache_prices_remain_unknown() {
    // Given
    let json = r#"{"id":"y","cost":{"input":2}}"#;
    // When
    let model: ModelMetadata = serde_json::from_str(json).expect("metadata parses");
    // Then
    assert_eq!(
        (model.cache_read_price, model.cache_write_price),
        (None, None)
    );
}
