use super::*;

#[tokio::test]
async fn ac4_chunked_body_accepts_exactly_48_mib_and_stops_at_first_excess_byte() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    for excess in [false, true] {
        let polled = AtomicUsize::new(0);
        let chunks = (0..)
            .map(|index| {
                polled.fetch_add(1, Ordering::SeqCst);
                let len = if index == 48 { 1 } else { 1024 * 1024 };
                Ok(bytes::Bytes::from(vec![b' '; len]))
            })
            .take(if excess { 50 } else { 48 });
        let result = bounded_stream(futures_util::stream::iter(chunks)).await;
        if excess {
            assert!(
                matches!(result, Err(ProviderError::InvalidJson {detail}) if detail.contains("48 MiB"))
            );
            assert_eq!(
                polled.load(Ordering::SeqCst),
                49,
                "must not consume later chunks"
            );
        } else {
            assert_eq!(result.unwrap().len(), MAX_RESPONSE_BYTES);
        }
    }
}
