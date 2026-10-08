//! Browser OAuth worker: bounded loopback callback, manual fallback and cancellation.
use super::{LoginCommand, LoginEvent};
use providers::provider::{
    claude::{ClaudeOAuthClient, ClaudeOAuthConfig, ClaudeTokenStore},
    cursor::{CursorOAuthClient, CursorOAuthConfig, CursorTokenStore},
};
use std::{
    sync::{Arc, mpsc},
    time::Duration,
};

pub(super) async fn claude_login(
    store: Arc<dyn sandbox::CredentialStore>,
    account: String,
    events: &mpsc::Sender<LoginEvent>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<LoginCommand>,
) -> Result<u64, &'static str> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Manual callback remains available when the loopback port is occupied.
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 54545))
        .await
        .ok();
    let client = ClaudeOAuthClient::new(ClaudeOAuthConfig::default())
        .map_err(|_| "Could not initialize Claude login")?;
    let login = client.begin().map_err(|_| "Could not start Claude login")?;
    let expected_state = login.state.clone();
    events
        .send(LoginEvent::Prompt(login.authorize_url.clone(), true))
        .map_err(|_| "Login cancelled")?;
    let input = tokio::time::timeout(Duration::from_secs(15 * 60), async {
        loop {
            tokio::select! {
                command = commands.recv() => return match command { Some(LoginCommand::Code(code)) => Ok(code), _ => Err("Login cancelled") },
                connection = async { match &listener { Some(listener) => listener.accept().await, None => std::future::pending().await } } => {
                    let Ok((mut stream, peer)) = connection else { continue; };
                    if !peer.ip().is_loopback() { continue; }
                    let mut bytes = Vec::new(); let mut chunk = [0_u8; 1024];
                    // Bound a stalled/malicious local callback separately from the login deadline.
                    let read = tokio::time::timeout(Duration::from_secs(5), async {
                        while bytes.len() <= 8192 && !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                            let n = stream.read(&mut chunk).await.map_err(|_| ())?;
                            if n == 0 { break; } bytes.extend_from_slice(&chunk[..n]);
                        }
                        Ok::<_, ()>(())
                    }).await;
                    let callback = read.ok().and_then(Result::ok).and_then(|()| callback_url(&bytes, &expected_state));
                    let status = if callback.is_some() { "200 OK" } else { "400 Bad Request" };
                    let body = if callback.is_some() { "Authorization received. Return to evorch." } else { "Invalid callback. Return to evorch and retry." };
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    let _ = stream.write_all(response.as_bytes()).await;
                    if let Some(callback) = callback { return Ok(sandbox::Secret::from(callback)); }
                }
            }
        }
    }).await.map_err(|_| "Browser sign-in timed out; retry login")??;
    let bundle = tokio::select! {
        result = client.complete(login, input.expose()) => result.map_err(|_| "Claude authorization failed; retry login")?,
        _ = commands.recv() => return Err("Login cancelled"),
    };
    let tokens = routing::factory::CredentialStoreClaudeTokenStore::new(store, account);
    let lock = tokens.refresh_lock();
    let _guard = tokio::select! { guard = lock.lock() => guard, _ = commands.recv() => return Err("Login cancelled") };
    if matches!(
        commands.try_recv(),
        Ok(LoginCommand::Cancel) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ) {
        return Err("Login cancelled");
    }
    tokens
        .save(&bundle)
        .map_err(|_| "Could not save Claude credentials")?;
    Ok(bundle.expires_at)
}
pub(super) fn callback_url(request: &[u8], state: &str) -> Option<String> {
    if request.len() > 8192 {
        return None;
    }
    let line = std::str::from_utf8(request).ok()?.lines().next()?;
    let mut parts = line.split_whitespace();
    if parts.next()? != "GET" {
        return None;
    }
    let target = parts.next()?;
    if !target.starts_with("/callback?") {
        return None;
    }
    let url = url::Url::parse(&format!("http://localhost:54545{target}")).ok()?;
    let states: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key == "state")
        .collect();
    let codes: Vec<_> = url.query_pairs().filter(|(key, _)| key == "code").collect();
    if states.len() != 1
        || states[0].1 != state
        || codes.len() != 1
        || codes[0].1.is_empty()
        || url.query_pairs().any(|(key, _)| key == "error")
    {
        return None;
    }
    Some(url.into())
}
pub(super) async fn cursor_login(
    store: Arc<dyn sandbox::CredentialStore>,
    account: String,
    events: &mpsc::Sender<LoginEvent>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<LoginCommand>,
) -> Result<u64, &'static str> {
    let client = CursorOAuthClient::new(CursorOAuthConfig::default())
        .map_err(|_| "Could not initialize Cursor login")?;
    let request = client.begin().map_err(|_| "Could not start Cursor login")?;
    events
        .send(LoginEvent::Prompt(request.authorization_url.clone(), false))
        .map_err(|_| "Login cancelled")?;
    let bundle = tokio::select! {
        result = client.complete(&request) => result.map_err(|_| "Cursor authorization failed; retry login")?,
        _ = commands.recv() => return Err("Login cancelled"),
    };
    let tokens = routing::factory::CredentialStoreCursorTokenStore::new(store, account);
    let lock = tokens.refresh_lock();
    let _guard = tokio::select! { guard = lock.lock() => guard, _ = commands.recv() => return Err("Login cancelled") };
    if matches!(
        commands.try_recv(),
        Ok(LoginCommand::Cancel) | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected)
    ) {
        return Err("Login cancelled");
    }
    tokens
        .save(&bundle)
        .map_err(|_| "Could not save Cursor credentials")?;
    Ok(bundle.expires_at)
}
