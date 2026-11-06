use super::wire;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use serde_json::json;
#[test]
fn search_wire_helpers_preserve_existing_contract() {
    let mut headers = HeaderMap::new();
    headers.insert("x-tenant", HeaderValue::from_static("fixture"));
    headers.insert(ACCEPT, HeaderValue::from_static("wrong"));
    let headers = wire::accept_headers(headers);
    assert_eq!(headers[ACCEPT], "application/json, text/event-stream");
    assert_eq!(headers["x-tenant"], "fixture");
    assert_eq!(
        wire::request(4, "tools/call", json!({"name":"search"})),
        json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"search"}})
    );
    let single = wire::response_frames(None, br#"{"id":99,"result":{"ok":true}}"#).unwrap();
    assert_eq!(wire::search_response(single, 4).unwrap()["id"], 99);
    let body = b"data: {\"id\":99}\n\ndata: {\"id\":4,\"result\":{}}\n\n";
    let frames = wire::response_frames(Some("text/event-stream"), body).unwrap();
    assert_eq!(wire::search_response(frames, 4).unwrap()["id"], 4);
    assert!(wire::search_response(vec![json!({"id":1}), json!({"id":2})], 4).is_err());
}
