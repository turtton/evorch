//! The real providers behind [`LoginStarter`]: Codex and Claude read their
//! callback from a loopback listener of the core host (or from a pasted
//! redirect URL), Cursor polls the provider until the person finished in the
//! browser. The token exchange and the credential store are the same code the
//! runs use (`providers`, `routing`).
use super::login::{BoxFuture, Callback, LoginFailure, LoginFlow, LoginStarter};
use providers::{
    ProviderError,
    provider::{
        claude::{
            ClaudeLoginRequest, ClaudeOAuthClient, ClaudeOAuthConfig, ClaudeOAuthError,
            ClaudeTokenStore,
        },
        codex::{
            oauth::{
                AuthorizeUrl, BrowserAuthClient, BrowserAuthError, BrowserAuthRequest,
                CallbackServer,
            },
            tokens::{CodexTokenStore, parse_jwt_claims},
        },
        cursor::{CursorLoginRequest, CursorOAuthClient, CursorOAuthConfig, CursorTokenStore},
    },
};
use routing::factory::{
    CredentialStoreClaudeTokenStore, CredentialStoreCursorTokenStore, CredentialStoreTokenStore,
    DEFAULT_AUTH_BASE_URL,
};
use sandbox::CredentialStore;
use std::{
    io::{Read, Write},
    net::{Ipv4Addr, SocketAddr, TcpListener},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use workbench_protocol::namespaces::auth::{LoginInputRejection, LoginMethod};

const CODEX_PORTS: [u16; 2] = [1455, 1457];
const CLAUDE_PORT: u16 = 54545;
/// How long a blocking listener goes without looking at its stop flag.
const LISTENER_SLICE: Duration = Duration::from_millis(100);

pub struct ProviderLogins {
    credentials: Arc<dyn CredentialStore>,
}

impl ProviderLogins {
    pub fn new(credentials: Arc<dyn CredentialStore>) -> Self {
        Self { credentials }
    }
}

impl LoginStarter for ProviderLogins {
    fn begin(
        &self,
        method: LoginMethod,
        account: &str,
    ) -> Result<Arc<dyn LoginFlow>, LoginFailure> {
        let store = self.credentials.clone();
        let account = account.to_owned();
        Ok(match method {
            LoginMethod::Codex => Arc::new(CodexFlow::begin(store, account)?),
            LoginMethod::Claude => Arc::new(ClaudeFlow::begin(store, account)?),
            LoginMethod::Cursor => Arc::new(CursorFlow::begin(store, account)?),
        })
    }
}

fn provider_failure(error: &ProviderError) -> LoginFailure {
    match error {
        ProviderError::Timeout
        | ProviderError::Request(_)
        | ProviderError::Transport { .. }
        | ProviderError::RetriesExhausted { .. }
        | ProviderError::RateLimited { .. } => LoginFailure::Network,
        ProviderError::Http { .. } => LoginFailure::Rejected,
        _ => LoginFailure::Unavailable,
    }
}

fn browser_failure(error: &BrowserAuthError) -> LoginFailure {
    match error {
        BrowserAuthError::CallbackPortBusy => LoginFailure::PortBusy,
        BrowserAuthError::Timeout => LoginFailure::Network,
        BrowserAuthError::Rejected => LoginFailure::Rejected,
        BrowserAuthError::Provider(error) => provider_failure(error),
        BrowserAuthError::Io(_) | BrowserAuthError::InvalidUrl => LoginFailure::Unavailable,
    }
}

fn claude_failure(error: &ClaudeOAuthError) -> LoginFailure {
    match error {
        ClaudeOAuthError::Timeout | ClaudeOAuthError::Transport => LoginFailure::Network,
        ClaudeOAuthError::Callback
        | ClaudeOAuthError::ReauthenticationRequired
        | ClaudeOAuthError::HttpStatus(_) => LoginFailure::Rejected,
        ClaudeOAuthError::Configuration | ClaudeOAuthError::Protocol => LoginFailure::Unavailable,
    }
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, LoginFailure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| LoginFailure::Unavailable)
}

/// The code of a redirect URL of `path` on the loopback host, after the
/// `state` of the login was compared. Any other URL, a repeated or missing
/// parameter, or an `error` parameter is `Malformed`.
fn code_of_redirect(
    url: &AuthorizeUrl,
    path: &str,
    state: &str,
) -> Result<String, LoginInputRejection> {
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(url.host_str(), Some("localhost" | "127.0.0.1"))
        || url.path() != path
    {
        return Err(LoginInputRejection::Malformed);
    }
    let (mut code, mut seen_state) = (None, None);
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" if code.is_none() => code = Some(value.into_owned()),
            "state" if seen_state.is_none() => seen_state = Some(value.into_owned()),
            "code" | "state" | "error" => return Err(LoginInputRejection::Malformed),
            _ => {}
        }
    }
    let seen_state = seen_state.ok_or(LoginInputRejection::Malformed)?;
    if seen_state != state {
        return Err(LoginInputRejection::StateMismatch);
    }
    code.filter(|code| !code.is_empty() && !code.chars().any(char::is_whitespace))
        .ok_or(LoginInputRejection::Malformed)
}

/// The request line of a callback received on a loopback socket.
fn read_request_target(stream: &mut std::net::TcpStream) -> Option<String> {
    stream.set_nonblocking(false).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 1024];
    while bytes.len() <= 8192 && !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    if bytes.len() > 8192 {
        return None;
    }
    let line = std::str::from_utf8(&bytes).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    (parts.next()? == "GET").then_some(())?;
    parts.next().map(str::to_owned)
}

fn respond(stream: &mut std::net::TcpStream, accepted: bool) {
    let (status, body) = if accepted {
        ("200 OK", "Authorization received. Return to evorch.")
    } else {
        (
            "400 Bad Request",
            "Invalid callback. Return to evorch and retry.",
        )
    };
    let _ = stream.set_write_timeout(Some(Duration::from_secs(1)));
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
}

// ---- Codex ----------------------------------------------------------------

struct CodexFlow {
    client: BrowserAuthClient,
    server: Arc<CallbackServer>,
    request: Mutex<Option<BrowserAuthRequest>>,
    authorize_url: String,
    state: String,
    store: Arc<CredentialStoreTokenStore>,
}

impl CodexFlow {
    fn begin(credentials: Arc<dyn CredentialStore>, account: String) -> Result<Self, LoginFailure> {
        let client = BrowserAuthClient::with_default_http(DEFAULT_AUTH_BASE_URL)
            .map_err(|error| provider_failure(&error))?;
        let server = CallbackServer::bind_ports(&CODEX_PORTS).map_err(|e| browser_failure(&e))?;
        let request = client.begin(&server).map_err(|e| browser_failure(&e))?;
        Ok(Self {
            authorize_url: request.authorize_url.clone(),
            state: request.state.clone(),
            client,
            server: Arc::new(server),
            request: Mutex::new(Some(request)),
            store: Arc::new(CredentialStoreTokenStore::new(credentials, account)),
        })
    }
}

impl LoginFlow for CodexFlow {
    fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    fn accepts_input(&self) -> bool {
        true
    }

    fn check_input(&self, input: &str) -> Result<Callback, LoginInputRejection> {
        let url = AuthorizeUrl::parse(input.trim()).map_err(|_| LoginInputRejection::Malformed)?;
        code_of_redirect(&url, "/auth/callback", &self.state).map(Callback::new)
    }

    fn wait_loopback(
        &self,
        stop: Arc<AtomicBool>,
    ) -> BoxFuture<'_, Result<Callback, LoginFailure>> {
        let server = self.server.clone();
        let state = self.state.clone();
        Box::pin(async move {
            blocking(move || {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return Err(LoginFailure::Unavailable);
                    }
                    match server.wait_for_code(&state, LISTENER_SLICE) {
                        Ok(code) => return Ok(Callback::new(code)),
                        // A stray or wrong request does not end the login.
                        Err(BrowserAuthError::Timeout | BrowserAuthError::Rejected) => {}
                        Err(error) => return Err(browser_failure(&error)),
                    }
                }
            })
            .await?
        })
    }

    fn complete(&self, callback: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
        Box::pin(async move {
            let request = self
                .request
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or(LoginFailure::Unavailable)?;
            let bundle = self
                .client
                .complete(request, callback.expose())
                .await
                .map_err(|e| browser_failure(&e))?;
            let expires = parse_jwt_claims(&bundle.id_token)
                .map_err(|_| LoginFailure::Unavailable)?
                .exp;
            let store = self.store.clone();
            blocking(move || store.save(&bundle))
                .await?
                .map_err(|_| LoginFailure::StoreUnavailable)?;
            Ok(Some(expires))
        })
    }
}

// ---- Claude ---------------------------------------------------------------

struct ClaudeFlow {
    client: ClaudeOAuthClient,
    listener: Option<Arc<TcpListener>>,
    login: Mutex<Option<ClaudeLoginRequest>>,
    authorize_url: String,
    state: String,
    tokens: Arc<CredentialStoreClaudeTokenStore>,
}

impl ClaudeFlow {
    fn begin(credentials: Arc<dyn CredentialStore>, account: String) -> Result<Self, LoginFailure> {
        let client =
            ClaudeOAuthClient::new(ClaudeOAuthConfig::default()).map_err(|e| claude_failure(&e))?;
        let login = client.begin().map_err(|e| claude_failure(&e))?;
        // A busy port leaves the pasted input as the way to finish.
        let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, CLAUDE_PORT)))
            .ok()
            .filter(|listener| listener.set_nonblocking(true).is_ok())
            .map(Arc::new);
        Ok(Self {
            authorize_url: login.authorize_url.clone(),
            state: login.state.clone(),
            client,
            listener,
            login: Mutex::new(Some(login)),
            tokens: Arc::new(CredentialStoreClaudeTokenStore::new(credentials, account)),
        })
    }

    fn normalized(code: &str, state: &str) -> Callback {
        Callback::new(format!("{code}#{state}"))
    }
}

impl LoginFlow for ClaudeFlow {
    fn authorize_url(&self) -> &str {
        &self.authorize_url
    }

    fn accepts_input(&self) -> bool {
        true
    }

    fn check_input(&self, input: &str) -> Result<Callback, LoginInputRejection> {
        let input = input.trim();
        if let Ok(url) = AuthorizeUrl::parse(input) {
            return code_of_redirect(&url, "/callback", &self.state)
                .map(|code| Self::normalized(&code, &self.state));
        }
        // The console shows `code#state`. A bare code has no state to check.
        let (code, state) = input
            .split_once('#')
            .ok_or(LoginInputRejection::Malformed)?;
        if code.is_empty() || code.chars().any(char::is_whitespace) {
            return Err(LoginInputRejection::Malformed);
        }
        if state != self.state {
            return Err(LoginInputRejection::StateMismatch);
        }
        Ok(Self::normalized(code, state))
    }

    fn wait_loopback(
        &self,
        stop: Arc<AtomicBool>,
    ) -> BoxFuture<'_, Result<Callback, LoginFailure>> {
        let Some(listener) = self.listener.clone() else {
            return Box::pin(std::future::pending());
        };
        let state = self.state.clone();
        Box::pin(async move {
            blocking(move || {
                loop {
                    if stop.load(Ordering::SeqCst) {
                        return Err(LoginFailure::Unavailable);
                    }
                    match listener.accept() {
                        Ok((mut stream, peer)) => {
                            if !peer.ip().is_loopback() {
                                continue;
                            }
                            let code = read_request_target(&mut stream).and_then(|target| {
                                let url = AuthorizeUrl::parse(&format!("http://localhost{target}"))
                                    .ok()?;
                                code_of_redirect(&url, "/callback", &state).ok()
                            });
                            respond(&mut stream, code.is_some());
                            if let Some(code) = code {
                                return Ok(Self::normalized(&code, &state));
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(LISTENER_SLICE.min(Duration::from_millis(25)));
                        }
                        Err(_) => return Err(LoginFailure::Unavailable),
                    }
                }
            })
            .await?
        })
    }

    fn complete(&self, callback: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
        Box::pin(async move {
            let login = self
                .login
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or(LoginFailure::Unavailable)?;
            let bundle = self
                .client
                .complete(login, callback.expose())
                .await
                .map_err(|e| claude_failure(&e))?;
            let expires = bundle.expires_at;
            let lock = self.tokens.refresh_lock();
            let _guard = lock.lock().await;
            let tokens = self.tokens.clone();
            blocking(move || tokens.save(&bundle))
                .await?
                .map_err(|_| LoginFailure::StoreUnavailable)?;
            Ok(Some(expires))
        })
    }
}

// ---- Cursor ---------------------------------------------------------------

struct CursorFlow {
    client: CursorOAuthClient,
    request: CursorLoginRequest,
    bundle: Mutex<Option<providers::provider::cursor::CursorTokenBundle>>,
    tokens: Arc<CredentialStoreCursorTokenStore>,
}

impl CursorFlow {
    fn begin(credentials: Arc<dyn CredentialStore>, account: String) -> Result<Self, LoginFailure> {
        let client = CursorOAuthClient::new(CursorOAuthConfig::default())
            .map_err(|e| provider_failure(&e))?;
        let request = client.begin().map_err(|e| provider_failure(&e))?;
        Ok(Self {
            client,
            request,
            bundle: Mutex::new(None),
            tokens: Arc::new(CredentialStoreCursorTokenStore::new(credentials, account)),
        })
    }
}

impl LoginFlow for CursorFlow {
    fn authorize_url(&self) -> &str {
        &self.request.authorization_url
    }

    fn accepts_input(&self) -> bool {
        false
    }

    fn check_input(&self, _input: &str) -> Result<Callback, LoginInputRejection> {
        Err(LoginInputRejection::NotWaiting)
    }

    /// The provider is polled until the person finished in the browser; that
    /// poll is the part that waits for a person.
    fn wait_loopback(
        &self,
        _stop: Arc<AtomicBool>,
    ) -> BoxFuture<'_, Result<Callback, LoginFailure>> {
        Box::pin(async move {
            let bundle = self
                .client
                .complete(&self.request)
                .await
                .map_err(|e| provider_failure(&e))?;
            *self.bundle.lock().unwrap_or_else(PoisonError::into_inner) = Some(bundle);
            Ok(Callback::new("polled"))
        })
    }

    fn complete(&self, _callback: Callback) -> BoxFuture<'_, Result<Option<u64>, LoginFailure>> {
        Box::pin(async move {
            let bundle = self
                .bundle
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
                .ok_or(LoginFailure::Unavailable)?;
            let expires = bundle.expires_at;
            let lock = self.tokens.refresh_lock();
            let _guard = lock.lock().await;
            let tokens = self.tokens.clone();
            blocking(move || tokens.save(&bundle))
                .await?
                .map_err(|_| LoginFailure::StoreUnavailable)?;
            Ok(Some(expires))
        })
    }
}

/// When the id token of a stored Codex login expires.
pub(super) fn exp_of(id_token: &str) -> Option<u64> {
    parse_jwt_claims(id_token).ok().map(|claims| claims.exp)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> AuthorizeUrl {
        AuthorizeUrl::parse(text).unwrap()
    }

    #[test]
    fn a_redirect_is_read_only_for_its_path_and_state() {
        let ok = url("http://localhost:1455/auth/callback?code=abc&state=s1");
        assert_eq!(
            code_of_redirect(&ok, "/auth/callback", "s1").unwrap(),
            "abc"
        );
        assert_eq!(
            code_of_redirect(&ok, "/auth/callback", "s2"),
            Err(LoginInputRejection::StateMismatch)
        );
        for bad in [
            "http://localhost:1455/other?code=abc&state=s1",
            "http://evil.example/auth/callback?code=abc&state=s1",
            "http://localhost:1455/auth/callback?code=abc",
            "http://localhost:1455/auth/callback?state=s1",
            "http://localhost:1455/auth/callback?code=a&code=b&state=s1",
            "http://localhost:1455/auth/callback?code=abc&state=s1&error=denied",
            "ftp://localhost/auth/callback?code=abc&state=s1",
        ] {
            assert_eq!(
                code_of_redirect(&url(bad), "/auth/callback", "s1"),
                Err(LoginInputRejection::Malformed),
                "{bad}"
            );
        }
    }
}
