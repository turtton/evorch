use futures_util::StreamExt;
use tokio::io::{AsyncReadExt, AsyncWriteExt, copy_bidirectional};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

use super::*;
use crate::provider::claude::{ClaudeReasoningControls, ClaudeWireProfile};
mod support;
use support::{TlsServer, normalize_hello, read_request, send_response};

fn post(transport: &ClaudeTransport, url: &str) -> wreq::RequestBuilder {
    transport
        .request(
            wreq::Method::POST,
            url,
            &ProviderAuth::new("dummy-local-oauth"),
            ClaudeWireProfile
                .headers(
                    "33333333-3333-4333-8333-333333333333",
                    &ClaudeReasoningControls::default(),
                )
                .unwrap(),
        )
        .body("{}")
}

fn assert_native_request_shape(hello: Vec<u8>, requests: &[Vec<u8>]) {
    let expected = include_bytes!(
        "../../../../tests/fixtures/claude-transport/native-2.1.295-client-hello.bin"
    );
    assert_eq!(normalize_hello(hello), expected);
    let expected_headers: Vec<String> = serde_json::from_str(include_str!(
        "../../../../tests/fixtures/claude-transport/native-2.1.295-shared-headers.json"
    ))
    .unwrap();
    for raw in requests {
        assert_eq!(header_names(raw), expected_headers);
    }
}

fn header_names(raw: &[u8]) -> Vec<&str> {
    std::str::from_utf8(raw)
        .unwrap()
        .split("\r\n\r\n")
        .next()
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| line.split_once(':').unwrap().0)
        .collect()
}

#[tokio::test]
async fn native_client_hello_and_http_header_order_match_and_connection_is_reused() {
    let server = TlsServer::new().await;
    let transport = server.client(true, "api.anthropic.com");
    let url = server.url("api.anthropic.com");
    let task = tokio::spawn(async move {
        let (mut stream, hello) = server.accept().await.unwrap();
        let mut requests = Vec::new();
        for _ in 0..3 {
            requests.push(read_request(&mut stream).await);
            send_response(
                &mut stream,
                "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: keep-alive\r\n\r\n",
                b"{}",
            )
            .await;
        }
        let captured = hello.lock().unwrap().clone();
        (captured, requests)
    });
    for index in 0..3 {
        let mut request = post(&transport, &url);
        if index > 0 {
            let observation = crate::ObservationContext {
                attempts: None,
                run_id: if index == 1 { "child" } else { "nested" }.into(),
                turn: Some(crate::RequestTurnContext {
                    parent_subagent_id: (index == 2).then(|| "child".into()),
                    ..crate::RequestTurnContext::unattributed(true)
                }),
                ..Default::default()
            };
            let context = crate::provider::claude::ClaudeTransportContext::default()
                .begin(
                    "33333333-3333-4333-8333-333333333333",
                    "device",
                    Some(&observation),
                )
                .unwrap();
            let mut headers = ClaudeWireProfile
                .headers(
                    "33333333-3333-4333-8333-333333333333",
                    &ClaudeReasoningControls::default(),
                )
                .unwrap();
            context.apply_headers(&mut headers).unwrap();
            request = request.headers(headers);
        }
        assert_eq!(request.send().await.unwrap().bytes().await.unwrap(), "{}");
    }
    let (hello, requests) = task.await.unwrap();
    assert_native_request_shape(hello, &requests[..1]);
    let child_headers: std::collections::HashMap<String, Vec<String>> =
        serde_json::from_str(include_str!(
            "../../../../tests/fixtures/claude-transport/native-2.1.295-child-headers.json"
        ))
        .unwrap();
    assert_eq!(header_names(&requests[1]), child_headers["first_level"]);
    assert_eq!(header_names(&requests[2]), child_headers["nested"]);
}

#[tokio::test]
async fn connect_proxy_preserves_native_shape_without_receiving_oauth_credentials() {
    let server = TlsServer::new().await;
    let origin = server.addr();
    let proxy = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let transport = server.client_via_proxy(proxy.local_addr().unwrap());
    let server_task = tokio::spawn(async move {
        let (mut stream, hello) = server.accept().await.unwrap();
        let request = read_request(&mut stream).await;
        send_response(
            &mut stream,
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n",
            b"{}",
        )
        .await;
        stream.shutdown().await.unwrap();
        let captured = hello.lock().unwrap().clone();
        (captured, request)
    });
    let proxy_task = tokio::spawn(async move {
        let (mut downstream, _) = proxy.accept().await.unwrap();
        let connect = String::from_utf8(read_request(&mut downstream).await).unwrap();
        assert_eq!(
            connect.lines().next(),
            Some("CONNECT api.anthropic.com:443 HTTP/1.1")
        );
        for name in connect
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(':').map(|(name, _)| name))
        {
            assert!(!name.eq_ignore_ascii_case("authorization"));
            assert!(!name.eq_ignore_ascii_case("proxy-authorization"));
        }
        assert!(!connect.contains("dummy-local-oauth"));
        let mut upstream = TcpStream::connect(origin).await.unwrap();
        downstream
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .unwrap();
        downstream.flush().await.unwrap();
        copy_bidirectional(&mut downstream, &mut upstream)
            .await
            .unwrap();
    });
    let response = post(
        &transport,
        "https://api.anthropic.com/v1/messages?beta=true",
    )
    .send()
    .await
    .unwrap()
    .bytes()
    .await
    .unwrap();
    assert_eq!(response, "{}");
    drop(transport);
    let (hello, request) = server_task.await.unwrap();
    proxy_task.await.unwrap();
    assert!(
        std::str::from_utf8(&request)
            .unwrap()
            .contains("\r\nAuthorization: Bearer dummy-local-oauth\r\n")
    );
    assert_native_request_shape(hello, &[request]);
}

#[tokio::test]
async fn certificate_and_hostname_verification_fail_before_any_http_request() {
    for (trusted, host) in [(false, "api.anthropic.com"), (true, "wrong-host.test")] {
        let server = TlsServer::new().await;
        let transport = server.client(trusted, host);
        let url = server.url(host);
        let task = tokio::spawn(async move {
            assert!(server.accept().await.is_err());
        });
        let error = post(&transport, &url).send().await.unwrap_err();
        assert_eq!(
            map_transport_error(error),
            ProviderError::Transport {
                message: "Claude transport failed".into()
            }
        );
        task.await.unwrap();
    }
    assert!(additional_roots(b"not a certificate").is_err());
}

#[tokio::test]
async fn redirect_is_returned_without_forwarding_oauth_credentials() {
    let server = TlsServer::new().await;
    let transport = server.client(true, "api.anthropic.com");
    let url = server.url("api.anthropic.com");
    let task = tokio::spawn(async move {
        let (mut stream, _) = server.accept().await.unwrap();
        read_request(&mut stream).await;
        send_response(&mut stream, "HTTP/1.1 302 Found\r\nLocation: https://redirect.invalid/secret\r\nContent-Length: 0\r\n\r\n", b"").await;
        stream.shutdown().await.unwrap();
    });
    assert_eq!(post(&transport, &url).send().await.unwrap().status(), 302);
    task.await.unwrap();
}

#[tokio::test]
async fn a_disconnected_post_is_not_retried_inside_the_transport() {
    let server = TlsServer::new().await;
    let transport = server.client(true, "api.anthropic.com");
    let url = server.url("api.anthropic.com");
    let (finished, mut done) = oneshot::channel();
    let task = tokio::spawn(async move {
        let mut requests = 0;
        loop {
            tokio::select! {
                _ = &mut done => return requests,
                accepted = server.accept() => {
                    let (mut stream, _) = accepted.unwrap();
                    read_request(&mut stream).await;
                    requests += 1;
                }
            }
        }
    });
    assert!(post(&transport, &url).send().await.is_err());
    finished.send(()).unwrap();
    assert_eq!(task.await.unwrap(), 1);
}

#[tokio::test]
async fn cancellation_and_timeouts_before_headers_and_during_sse_close_the_connection() {
    for (after_headers, time_out) in [(false, false), (true, false), (false, true), (true, true)] {
        let server = TlsServer::new().await;
        let transport = server.client(true, "api.anthropic.com");
        let url = server.url("api.anthropic.com");
        let (received, ready) = oneshot::channel();
        let task = tokio::spawn(async move {
            let (mut stream, _) = server.accept().await.unwrap();
            read_request(&mut stream).await;
            received.send(()).unwrap();
            if after_headers {
                let body = b"data: {\"delta\":\"first\"}\n\n";
                send_response(&mut stream, &format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",body.len()),body).await;
                stream.write_all(b"\r\n").await.unwrap();
                stream.flush().await.unwrap();
            }
            let result = stream.read(&mut [0; 1]).await;
            assert!(
                matches!(result, Ok(0) | Err(_)),
                "termination must close TLS"
            );
        });
        let request = post(&transport, &url);
        let request = if time_out && !after_headers {
            request.timeout(Duration::from_secs(60))
        } else {
            request
        };
        let mut pending = Box::pin(request.send());
        if after_headers {
            let response = pending.await.unwrap();
            ready.await.unwrap();
            let mut stream = response.bytes_stream();
            assert_eq!(
                stream.next().await.unwrap().unwrap(),
                "data: {\"delta\":\"first\"}\n\n"
            );
            if time_out {
                let error = {
                    let mut next = Box::pin(stream.next());
                    assert!(futures_util::poll!(&mut next).is_pending());
                    tokio::time::pause();
                    tokio::time::advance(READ_TIMEOUT + Duration::from_secs(1)).await;
                    next.await.unwrap().unwrap_err()
                };
                assert_eq!(map_transport_error(error), ProviderError::Timeout);
                tokio::time::resume();
            }
            drop(stream);
        } else {
            tokio::select! {
                response = &mut pending => panic!("server has not sent headers: {response:?}"),
                _ = ready => {},
            }
            if time_out {
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(61)).await;
                assert_eq!(
                    map_transport_error(pending.await.unwrap_err()),
                    ProviderError::Timeout
                );
                tokio::time::resume();
            } else {
                drop(pending);
            }
        }
        task.await.unwrap();
    }
}
