//! 契約テスト共通のサポートユーティリティ。
//!
//! 各統合テストバイナリから `mod support;` で取り込んで使う。

use std::path::Path;

use event_bus::{Event, EventKind, EventReceiver, ProviderEvent, UsageEvent};
use wiremock::ResponseTemplate;

/// `tests/fixtures/<provider>/<name>` の内容を読み込んで返す。
///
/// # Panics
/// フィクスチャが存在しない・読めない場合にパニックする (テスト失敗として扱う)。
pub fn fixture(provider: &str, name: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(provider)
        .join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|err| panic!("フィクスチャの読み込みに失敗しました: {path:?}: {err}"))
}

/// SSE 本文を返す 200 応答テンプレートを生成する。
///
/// Content-Type は `text/event-stream`。
pub fn sse_response(body: &str) -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
}

/// 指定ステータスの JSON 応答テンプレートを生成する。
///
/// Content-Type は `application/json`。
pub fn json_response(status: u16, body: &str) -> ResponseTemplate {
    ResponseTemplate::new(status).set_body_raw(body, "application/json")
}

/// バスから次の usage イベントを受信して返す。
///
/// # Panics
/// 受信に失敗した場合、または受信したイベントが [`EventKind::Usage`]
/// でない場合に文脈付きでパニックする。
pub async fn next_usage_event(rx: &mut EventReceiver) -> UsageEvent {
    match next_event(rx).await.kind {
        EventKind::Usage(usage) => usage,
        other => panic!("Usage イベントを期待しましたが、別のイベントを受信しました: {other:?}"),
    }
}

/// バスから次の provider イベントを受信して返す。
///
/// # Panics
/// 受信に失敗した場合、または受信したイベントが [`EventKind::Provider`]
/// でない場合に文脈付きでパニックする。
pub async fn next_provider_event(rx: &mut EventReceiver) -> ProviderEvent {
    match next_event(rx).await.kind {
        EventKind::Provider(provider) => provider,
        other => panic!("Provider イベントを期待しましたが、別のイベントを受信しました: {other:?}"),
    }
}

/// バスから次のイベントをタイムアウト付きで受信する。
///
/// # Panics
/// 1 秒以内に受信できない場合、または受信処理が失敗した場合にパニックする。
pub async fn next_event(rx: &mut EventReceiver) -> Event {
    tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
        .await
        .expect("1 秒以内にイベントを受信できる")
        .unwrap_or_else(|err| panic!("イベントの受信に失敗しました: {err:?}"))
}
