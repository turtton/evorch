//! The loopback HTTP proxy that carries every connection of the agent
//! browser. Chromium hands it host names rather than addresses, so the
//! policy's single resolution is the address the proxy dials: checked
//! addresses cannot be swapped by a second lookup (DNS rebinding), and
//! traffic the Fetch domain never pauses (WebSockets, worker and service
//! worker fetches) still meets the policy.

use std::convert::Infallible;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Empty, combinators::BoxBody};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode, header};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use super::{AgentError, RequestPolicy};

const TLS_HANDSHAKE_RECORD: u8 = 0x16;
const HOP_BY_HOP: &[&str] = &["proxy-connection", "proxy-authorization", "keep-alive"];

type Body = BoxBody<Bytes, hyper::Error>;
type Refusal = (StatusCode, String);

pub(super) struct Proxy {
    address: SocketAddr,
    accept: tokio::task::JoinHandle<()>,
}

impl Proxy {
    pub(super) async fn start(policy: Arc<dyn RequestPolicy>) -> Result<Self, AgentError> {
        let launch = |error: std::io::Error| AgentError::Launch(format!("agent proxy: {error}"));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(launch)?;
        let address = listener.local_addr().map_err(launch)?;
        let accept = tokio::spawn(async move {
            loop {
                let stream = match listener.accept().await {
                    Ok((stream, _)) => stream,
                    Err(error) => {
                        tracing::debug!(%error, "agent proxy accept failed");
                        continue;
                    }
                };
                let policy = Arc::clone(&policy);
                tokio::spawn(async move {
                    let service = hyper::service::service_fn(move |request| {
                        handle(Arc::clone(&policy), request)
                    });
                    if let Err(error) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), service)
                        .with_upgrades()
                        .await
                    {
                        tracing::debug!(%error, "agent proxy connection ended");
                    }
                });
            }
        });
        Ok(Self { address, accept })
    }

    pub(super) fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

async fn handle(
    policy: Arc<dyn RequestPolicy>,
    request: Request<Incoming>,
) -> Result<Response<Body>, Infallible> {
    let outcome = if request.method() == Method::CONNECT {
        tunnel(policy, request).await
    } else {
        forward(policy.as_ref(), request).await
    };
    Ok(outcome.unwrap_or_else(|(status, reason)| {
        tracing::debug!(%status, %reason, "agent proxy refused a connection");
        let mut response = Response::new(empty());
        *response.status_mut() = status;
        response
    }))
}

/// Opens a CONNECT tunnel. TLS tunnels use the addresses checked before the
/// `200`; any other payload (such as a `ws://` handshake) is checked again
/// as plain traffic once its first byte arrives.
async fn tunnel(
    policy: Arc<dyn RequestPolicy>,
    mut request: Request<Incoming>,
) -> Result<Response<Body>, Refusal> {
    let authority = request
        .uri()
        .authority()
        .ok_or((StatusCode::BAD_REQUEST, "CONNECT needs host:port".into()))?
        .clone();
    let host = authority.host().to_owned();
    let port = authority.port_u16().unwrap_or(443);
    let tls_addrs = policy
        .resolve(&host, port, true)
        .await
        .map_err(|reason| (StatusCode::FORBIDDEN, reason))?;
    let upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let mut client = match upgrade.await {
            Ok(upgraded) => TokioIo::new(upgraded),
            Err(error) => {
                tracing::debug!(%error, "agent proxy tunnel was not upgraded");
                return;
            }
        };
        let mut first = [0; 1];
        if client.read_exact(&mut first).await.is_err() {
            return;
        }
        let addrs = if first[0] == TLS_HANDSHAKE_RECORD {
            tls_addrs
        } else {
            match policy.resolve(&host, port, false).await {
                Ok(addrs) => addrs,
                Err(reason) => {
                    tracing::debug!(%host, port, %reason, "agent proxy refused a plain tunnel");
                    return;
                }
            }
        };
        let Ok(mut upstream) = dial(&addrs).await else {
            return;
        };
        if upstream.write_all(&first).await.is_ok() {
            let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
        }
    });
    Ok(Response::new(empty()))
}

async fn forward(
    policy: &dyn RequestPolicy,
    request: Request<Incoming>,
) -> Result<Response<Body>, Refusal> {
    if request.headers().contains_key(header::UPGRADE) {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            "upgrades must use CONNECT".into(),
        ));
    }
    let uri = request.uri();
    let (Some("http"), Some(host)) = (uri.scheme_str(), uri.host()) else {
        return Err((
            StatusCode::BAD_REQUEST,
            "expected an absolute http:// URI".into(),
        ));
    };
    let addrs = policy
        .resolve(host, uri.port_u16().unwrap_or(80), false)
        .await
        .map_err(|reason| (StatusCode::FORBIDDEN, reason))?;
    let upstream = dial(&addrs).await.map_err(bad_gateway)?;
    let (mut sender, connection) =
        hyper::client::conn::http1::handshake::<_, Incoming>(TokioIo::new(upstream))
            .await
            .map_err(bad_gateway)?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            tracing::debug!(%error, "agent proxy upstream connection ended");
        }
    });
    let (mut parts, body) = request.into_parts();
    parts.uri = parts
        .uri
        .path_and_query()
        .map_or("/", |path| path.as_str())
        .parse()
        .map_err(bad_gateway)?;
    for name in HOP_BY_HOP {
        parts.headers.remove(*name);
    }
    let response = sender
        .send_request(Request::from_parts(parts, body))
        .await
        .map_err(bad_gateway)?;
    Ok(response.map(BodyExt::boxed))
}

async fn dial(addrs: &[SocketAddr]) -> std::io::Result<TcpStream> {
    let mut last = std::io::Error::other("no address to connect to");
    for addr in addrs {
        match TcpStream::connect(addr).await {
            Ok(stream) => return Ok(stream),
            Err(error) => last = error,
        }
    }
    Err(last)
}

fn bad_gateway(error: impl std::fmt::Display) -> Refusal {
    (StatusCode::BAD_GATEWAY, error.to_string())
}

fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    struct Fixed {
        hosts: HashMap<&'static str, SocketAddr>,
        plain: bool,
    }

    #[async_trait::async_trait]
    impl RequestPolicy for Fixed {
        async fn check(&self, _url: &url::Url) -> Result<(), String> {
            Ok(())
        }

        async fn resolve(
            &self,
            host: &str,
            _port: u16,
            tls: bool,
        ) -> Result<Vec<SocketAddr>, String> {
            if !tls && !self.plain {
                return Err("plain traffic refused".into());
            }
            self.hosts
                .get(host)
                .map(|addr| vec![*addr])
                .ok_or_else(|| format!("{host} refused"))
        }
    }

    async fn proxy(hosts: &[(&'static str, SocketAddr)], plain: bool) -> Proxy {
        Proxy::start(Arc::new(Fixed {
            hosts: hosts.iter().copied().collect(),
            plain,
        }))
        .await
        .unwrap()
    }

    async fn send(proxy: &Proxy, head: &str) -> BufReader<TcpStream> {
        let mut stream = TcpStream::connect(proxy.address()).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        BufReader::new(stream)
    }

    async fn status_line(reader: &mut BufReader<TcpStream>) -> String {
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).await.unwrap();
            if header.trim().is_empty() {
                return line.trim_end().to_owned();
            }
        }
    }

    fn upstream() -> (std::net::TcpListener, SocketAddr) {
        let listener = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    #[tokio::test]
    async fn refused_destinations_are_forbidden_for_connect_and_plain_http() {
        let proxy = proxy(&[], true).await;

        let mut connect = send(
            &proxy,
            "CONNECT refused.test:443 HTTP/1.1\r\nHost: refused.test:443\r\n\r\n",
        )
        .await;
        let mut get = send(
            &proxy,
            "GET http://refused.test/ HTTP/1.1\r\nHost: refused.test\r\n\r\n",
        )
        .await;

        assert_eq!(status_line(&mut connect).await, "HTTP/1.1 403 Forbidden");
        assert_eq!(status_line(&mut get).await, "HTTP/1.1 403 Forbidden");
    }

    #[tokio::test]
    async fn connections_dial_the_address_the_policy_resolved() {
        let (listener, address) = upstream();
        let listener = TcpListener::from_std(listener).unwrap();
        let proxy = proxy(&[("pinned.test", address)], true).await;

        let mut tunnel = send(
            &proxy,
            "CONNECT pinned.test:443 HTTP/1.1\r\nHost: pinned.test:443\r\n\r\n",
        )
        .await;
        assert_eq!(status_line(&mut tunnel).await, "HTTP/1.1 200 OK");
        tunnel
            .write_all(&[TLS_HANDSHAKE_RECORD, 3, 1])
            .await
            .unwrap();
        let (mut tunneled, _) = listener.accept().await.unwrap();
        let mut hello = [0; 3];
        tunneled.read_exact(&mut hello).await.unwrap();
        assert_eq!(hello, [TLS_HANDSHAKE_RECORD, 3, 1]);

        let mut get = send(
            &proxy,
            "GET http://pinned.test/page?q=1 HTTP/1.1\r\nHost: pinned.test\r\nProxy-Connection: keep-alive\r\n\r\n",
        )
        .await;
        let (forwarded, _) = listener.accept().await.unwrap();
        let mut forwarded = BufReader::new(forwarded);
        let mut request = String::new();
        loop {
            let mut line = String::new();
            forwarded.read_line(&mut line).await.unwrap();
            if line.trim().is_empty() {
                break;
            }
            request.push_str(&line.to_ascii_lowercase());
        }
        forwarded
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok")
            .await
            .unwrap();
        assert_eq!(status_line(&mut get).await, "HTTP/1.1 200 OK");
        let mut body = [0; 2];
        get.read_exact(&mut body).await.unwrap();

        assert!(
            request.starts_with("get /page?q=1 http/1.1\r\n"),
            "{request}"
        );
        assert!(!request.contains("proxy-connection"), "{request}");
        assert_eq!(&body, b"ok");
    }

    #[tokio::test]
    async fn tunnels_without_tls_need_plain_traffic_permission() {
        let (listener, address) = upstream();
        let proxy = proxy(&[("pinned.test", address)], false).await;

        let mut tunnel = send(
            &proxy,
            "CONNECT pinned.test:80 HTTP/1.1\r\nHost: pinned.test:80\r\n\r\n",
        )
        .await;
        assert_eq!(status_line(&mut tunnel).await, "HTTP/1.1 200 OK");
        tunnel
            .write_all(b"GET / HTTP/1.1\r\nUpgrade: websocket\r\n\r\n")
            .await
            .unwrap();
        let mut rest = Vec::new();
        let closed = tunnel.read_to_end(&mut rest).await;

        assert!(closed.map_or(true, |read| read == 0), "{rest:?}");
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "the proxy must not dial a refused plain tunnel"
        );
    }
}
