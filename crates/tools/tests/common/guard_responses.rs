//! NetworkGuard 統合テスト専用の response 組み立てヘルパー。

use crate::common::response_with_status;

pub fn identity_response(body: &[u8]) -> Vec<u8> {
    response_with_headers(&[format!("Content-Length: {}", body.len())], body)
}

pub fn response_with_headers(headers: &[String], body: &[u8]) -> Vec<u8> {
    response_with_status("200 OK", headers, body)
}

pub fn redirect(location: &str) -> Vec<u8> {
    format!("HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").into_bytes()
}

pub fn chunked_response(chunks: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    let mut response =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    for chunk in chunks {
        response.extend_from_slice(format!("{:X}\r\n", chunk.len()).as_bytes());
        response.extend_from_slice(&chunk);
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"0\r\n\r\n");
    response
}
