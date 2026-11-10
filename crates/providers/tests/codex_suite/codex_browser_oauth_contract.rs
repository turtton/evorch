use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::Duration;

use providers::provider::codex::oauth::{
    BrowserAuthClient, BrowserAuthError, CODEX_CLIENT_ID, CODEX_SCOPE, CallbackServer, PkcePair,
};
use wiremock::matchers::{body_string_contains, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn callback() -> CallbackServer {
    CallbackServer::bind_addr(SocketAddr::from(([127, 0, 0, 1], 0))).expect("callback")
}

fn get(uri: &str, query: &str) -> String {
    let url = reqwest::Url::parse(uri).expect("URL");
    let mut stream = TcpStream::connect(("127.0.0.1", url.port().expect("port"))).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    write!(
        stream,
        "GET /auth/callback?{query} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .expect("request");
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("response");
    response
}

#[test]
fn authorize_url_contains_pkce_state_scope_and_simplified_flow() {
    // Given
    let server = callback();
    let client = BrowserAuthClient::with_default_http("https://auth.openai.com").expect("client");
    // When
    let request = client.begin(&server).expect("begin");
    let url = reqwest::Url::parse(&request.authorize_url).expect("authorize URL");
    let params: HashMap<_, _> = url.query_pairs().into_owned().collect();
    // Then
    assert_eq!(url.path(), "/oauth/authorize");
    assert_eq!(params["client_id"], CODEX_CLIENT_ID);
    assert_eq!(params["scope"], CODEX_SCOPE);
    assert_eq!(params["redirect_uri"], server.redirect_uri());
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["code_challenge_method"], "S256");
    assert_eq!(
        params["code_challenge"],
        PkcePair::challenge_for(request.code_verifier())
    );
    assert_eq!(params["state"], request.state);
    assert!(request.state.len() >= 32);
    assert_eq!(params["codex_cli_simplified_flow"], "true");
    assert_eq!(params["id_token_add_organizations"], "true");
    assert_eq!(params["originator"], "evorch");
    assert_ne!(
        client.begin(&server).expect("second begin").state,
        request.state
    );
}

#[test]
fn callback_server_accepts_matching_state_and_returns_code() {
    // Given
    let server = callback();
    let uri = server.redirect_uri();
    let browser = std::thread::spawn(move || get(&uri, "state=expected&code=a%2Bb"));
    // When
    let code = server
        .wait_for_code("expected", Duration::from_secs(2))
        .expect("code");
    // Then
    assert_eq!(code, "a+b");
    assert!(browser.join().expect("browser").starts_with("HTTP/1.1 200"));
}

#[test]
fn callback_server_rejects_state_mismatch_with_400() {
    // Given
    let server = callback();
    let uri = server.redirect_uri();
    let browser = std::thread::spawn(move || get(&uri, "state=wrong&code=stolen"));
    // When
    let result = server.wait_for_code("expected", Duration::from_secs(2));
    // Then
    assert!(matches!(result, Err(BrowserAuthError::Rejected)));
    assert!(browser.join().expect("browser").starts_with("HTTP/1.1 400"));
}

#[test]
fn callback_server_times_out_without_request() {
    // Given
    let server = callback();
    // When
    let result = server.wait_for_code("expected", Duration::from_millis(20));
    // Then
    assert!(matches!(result, Err(BrowserAuthError::Timeout)));
}

#[test]
fn bind_ports_falls_back_to_second_port_when_first_busy() {
    // Given: reserve first port, use an OS-assigned second port to isolate concurrent runs.
    let busy = TcpListener::bind("127.0.0.1:0").expect("busy port");
    let first = busy.local_addr().expect("address").port();
    // When
    let server = CallbackServer::bind_ports(&[first, 0]).expect("fallback");
    // Then
    let uri = reqwest::Url::parse(&server.redirect_uri()).expect("URI");
    assert_ne!(uri.port(), Some(first));
    assert!(matches!(
        CallbackServer::bind_ports(&[first]),
        Err(BrowserAuthError::CallbackPortBusy)
    ));
}

#[tokio::test]
async fn complete_exchanges_code_with_browser_redirect_uri() {
    // Given
    let issuer = MockServer::start().await;
    let server = callback();
    let client = BrowserAuthClient::with_default_http(issuer.uri()).expect("client");
    let request = client.begin(&server).expect("begin");
    let redirect = server
        .redirect_uri()
        .replace(':', "%3A")
        .replace('/', "%2F");
    Mock::given(method("POST"))
        .and(path("/oauth/token"))
        .and(body_string_contains(format!("redirect_uri={redirect}")))
        .and(body_string_contains(format!(
            "code_verifier={}",
            request.code_verifier()
        )))
        .and(body_string_contains("code=authorization-code"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "access_token": "access", "refresh_token": "refresh", "id_token": "id"
        })))
        .expect(1)
        .mount(&issuer)
        .await;
    // When
    let bundle = client
        .complete(request, "authorization-code")
        .await
        .expect("exchange");
    // Then
    assert_eq!(bundle.access_token, "access");
}
