use super::*;

#[tokio::test]
async fn streaming_displays_short_divergence_when_retry_completes() {
    // Given: Hello の後に EOF、短い Hi で再試行が完了する。
    let client = ScriptedClient::new(vec![
        Ok(vec![text("Hello")]),
        Ok(vec![text("Hi"), completed()]),
    ]);
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    // When: 実際の send_streaming で再試行する。
    let result = client
        .send_streaming(&ProviderAuth::new("key"), &sample_request(), &bus)
        .await;
    // Then: 表示済み内容は撤回せず、新しい短い内容も失わない。
    assert_eq!(result, Ok(sample_response()));
    assert_eq!(text_deltas(&mut receiver).concat(), "HelloHi");
    assert_eq!(client.attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn streaming_deduplicates_divergent_attempt_when_third_attempt_completes() {
    // Given: 二回目で分岐して再び EOF、三回目は二回目を再生する。
    let client = ScriptedClient::new(vec![
        Ok(vec![text("Hello")]),
        Ok(vec![text("Hi!!!")]),
        Ok(vec![text("Hi!!! world"), completed()]),
    ]);
    let bus = EventBus::new(16);
    let mut receiver = bus.subscribe();
    // When: 実際の send_streaming で三回試行する。
    let result = client
        .send_streaming(&ProviderAuth::new("key"), &sample_request(), &bus)
        .await;
    // Then: 分岐後の全文を累積表示と混同せず、続きだけ追加する。
    assert_eq!(result, Ok(sample_response()));
    assert_eq!(text_deltas(&mut receiver).concat(), "HelloHi!!! world");
    assert_eq!(client.attempts.load(Ordering::SeqCst), 3);
}
