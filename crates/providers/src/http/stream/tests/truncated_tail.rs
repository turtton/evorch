use super::*;

#[tokio::test]
async fn truncated_tail_ends_without_error_item() {
    // Given: JSON または UTF-8 の途中で切断された未終端フレーム。
    for tail in [b"data: {\"choices\":".as_slice(), b"data: \xe3\x81"] {
        let stream = adapt_sse_stream(
            futures_util::stream::iter(vec![Ok(Bytes::copy_from_slice(tail))]),
            FakeInterpreter::completing_on_tail(),
            UsageEmitter::new(None, "test"),
            "m".to_string(),
            observer(None),
        );
        // When: EOF まで読み取る。
        let items = collect(stream).await;
        // Then: エラー項目も、finish による偽の Completed も流さない。
        assert!(items.is_empty(), "{items:?}");
    }
}

#[tokio::test]
async fn complete_malformed_frame_still_delivers_invalid_json() {
    // Given: 空行で終端した不正 JSON の後に正常なフレームがある。
    let stream = adapt_sse_stream(
        futures_util::stream::iter(vec![Ok(Bytes::from_static(
            b"data: {bad}\n\ndata: [DONE]\n\n",
        ))]),
        FakeInterpreter::new(),
        UsageEmitter::new(None, "test"),
        "m".to_string(),
        observer(None),
    );
    // When: ストリームを収集する。
    let items = collect(stream).await;
    // Then: 完結済みの不正フレームは非リトライ対象のまま。
    assert!(matches!(
        items.as_slice(),
        [Err(ProviderError::InvalidJson { .. })]
    ));
}
