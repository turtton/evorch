use super::*;
use futures_util::poll;
use std::{io::ErrorKind, time::Duration};
use tokio::io::AsyncWriteExt;

const ENDPOINT: &str = "ws://127.0.0.1:9222/devtools/browser/test-id";
fn line(endpoint: &str) -> String {
    format!("DevTools listening on {endpoint}\n")
}
async fn parse_bytes(bytes: &[u8]) -> std::io::Result<String> {
    read(bytes, Instant::now() + Duration::from_secs(20)).await
}

#[tokio::test]
async fn loopback_browser_endpoints_only() {
    for endpoint in [
        ENDPOINT,
        "ws://[::1]:9222/devtools/browser/id",
        "ws://127.0.0.2:4/devtools/browser/id",
    ] {
        assert_eq!(
            parse_bytes(line(endpoint).as_bytes()).await.unwrap(),
            endpoint
        );
    }
    for endpoint in [
        "bogus",
        "http://127.0.0.1:9222/devtools/browser/id",
        "wss://127.0.0.1:9222/devtools/browser/id",
        "ws://example.org:9222/devtools/browser/id",
        "ws://localhost:9222/devtools/browser/id",
        "ws://192.0.2.1:9222/devtools/browser/id",
        "ws://[::]:9222/devtools/browser/id",
        "ws://0.0.0.0:9222/devtools/browser/id",
        "ws://127.0.0.1:0/devtools/browser/id",
        "ws://127.0.0.1/devtools/browser/id",
        "ws://user@127.0.0.1:9222/devtools/browser/id",
        "ws://127.0.0.1:9222/devtools/browser/",
        "ws://127.0.0.1:9222/devtools/page/id",
        "ws://127.0.0.1:9222/devtools/browser/id?query",
        "ws://127.0.0.1:9222/devtools/browser/id#fragment",
        "ws://127.0.0.1:9222/devtools/browser/id ",
    ] {
        assert_eq!(
            parse_bytes(line(endpoint).as_bytes())
                .await
                .unwrap_err()
                .kind(),
            ErrorKind::InvalidData,
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn eof_utf8_and_line_boundaries() {
    for bytes in [
        b"".as_slice(),
        b"some log\n",
        b"DevTools listening on ws://127.0.0.1:9222/devtools/browser/id",
    ] {
        assert_eq!(
            parse_bytes(bytes).await.unwrap_err().kind(),
            ErrorKind::UnexpectedEof
        );
    }
    assert_eq!(
        parse_bytes(b"invalid \xff\n").await.unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    let mut bytes = vec![b'a'; 16 * 1024 - 1];
    bytes.push(b'\n');
    bytes.extend(line(ENDPOINT).bytes());
    assert_eq!(parse_bytes(&bytes).await.unwrap(), ENDPOINT);
    let mut long = vec![b'a'; 16 * 1024];
    long.push(b'\n');
    long.extend(line(ENDPOINT).bytes());
    assert_eq!(
        parse_bytes(&long).await.unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    assert_eq!(
        parse_bytes(&vec![b'a'; 16 * 1024 + 1])
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

fn padded(total: usize) -> Vec<u8> {
    let endpoint = line(ENDPOINT);
    let mut bytes = Vec::new();
    while bytes.len() < total - endpoint.len() {
        let count = (total - endpoint.len() - bytes.len()).min(1024);
        bytes.extend(std::iter::repeat_n(b'a', count - 1));
        bytes.push(b'\n');
    }
    bytes.extend(endpoint.bytes());
    bytes
}
#[tokio::test]
async fn total_limit_is_256_kib_including_the_endpoint() {
    assert_eq!(parse_bytes(&padded(256 * 1024)).await.unwrap(), ENDPOINT);
    assert_eq!(
        parse_bytes(&padded(256 * 1024 + 1))
            .await
            .unwrap_err()
            .kind(),
        ErrorKind::InvalidData
    );
}

#[tokio::test(start_paused = true)]
async fn absolute_deadline_does_not_reset_on_stderr_progress() {
    let (reader, mut writer) = tokio::io::duplex(1024);
    let future = read(reader, Instant::now() + Duration::from_secs(20));
    tokio::pin!(future);
    assert!(poll!(&mut future).is_pending());
    writer.write_all(b"first log\n").await.unwrap();
    assert!(poll!(&mut future).is_pending());
    tokio::time::advance(Duration::from_secs(19)).await;
    writer.write_all(b"more progress\n").await.unwrap();
    assert!(poll!(&mut future).is_pending());
    tokio::time::advance(Duration::from_secs(1)).await;
    let std::task::Poll::Ready(result) = poll!(&mut future) else {
        panic!("deadline extended past 20 seconds");
    };
    assert_eq!(result.unwrap_err().kind(), ErrorKind::TimedOut);
}
#[tokio::test(start_paused = true)]
async fn delayed_endpoint_before_the_deadline_succeeds() {
    let (reader, mut writer) = tokio::io::duplex(1024);
    let future = read(reader, Instant::now() + Duration::from_secs(20));
    tokio::pin!(future);
    assert!(poll!(&mut future).is_pending());
    tokio::time::advance(Duration::from_secs(19)).await;
    writer.write_all(line(ENDPOINT).as_bytes()).await.unwrap();
    assert_eq!(future.await.unwrap(), ENDPOINT);
}
