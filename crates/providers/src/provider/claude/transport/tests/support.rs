use std::{
    io,
    net::{Ipv4Addr, SocketAddr},
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, pki_types::PrivateKeyDer},
};

use super::super::{ClaudeTransport, additional_roots, client_builder};

pub type Captured = Arc<Mutex<Vec<u8>>>;
pub type TlsStream = tokio_rustls::server::TlsStream<CapturingIo>;

pub struct TlsServer {
    listener: TcpListener,
    acceptor: TlsAcceptor,
    pem: String,
}

impl TlsServer {
    pub async fn new() -> Self {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["api.anthropic.com".into()]).unwrap();
        let pem = cert.pem();
        let mut config = ServerConfig::builder_with_provider(Arc::new(
            tokio_rustls::rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.into()],
            PrivateKeyDer::try_from(signing_key.serialize_der()).unwrap(),
        )
        .unwrap();
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Self {
            listener: TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap(),
            acceptor: TlsAcceptor::from(Arc::new(config)),
            pem,
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.listener.local_addr().unwrap()
    }

    pub fn url(&self, host: &str) -> String {
        format!(
            "https://{host}:{}/v1/messages?beta=true",
            self.addr().port()
        )
    }

    pub fn client(&self, trusted: bool, host: &str) -> ClaudeTransport {
        let mut builder = client_builder()
            .no_proxy()
            .resolve(host.to_owned(), self.addr());
        if trusted {
            builder = builder.tls_cert_store(additional_roots(self.pem.as_bytes()).unwrap());
        }
        ClaudeTransport {
            client: builder.build().unwrap(),
        }
    }

    pub fn client_via_proxy(&self, proxy: SocketAddr) -> ClaudeTransport {
        ClaudeTransport {
            client: client_builder()
                .no_proxy()
                .proxy(wreq::Proxy::https(format!("http://{proxy}")).unwrap())
                // Keep an accidental direct fallback on loopback as well.
                .resolve("api.anthropic.com", self.addr())
                .tls_cert_store(additional_roots(self.pem.as_bytes()).unwrap())
                .build()
                .unwrap(),
        }
    }

    pub async fn accept(&self) -> io::Result<(TlsStream, Captured)> {
        let (stream, _) = self.listener.accept().await?;
        let captured = Arc::new(Mutex::new(Vec::new()));
        let io = CapturingIo {
            stream,
            captured: captured.clone(),
        };
        self.acceptor
            .accept(io)
            .await
            .map(|stream| (stream, captured))
    }
}

pub struct CapturingIo {
    stream: TcpStream,
    captured: Captured,
}

impl AsyncRead for CapturingIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let before = buf.filled().len();
        let result = Pin::new(&mut self.stream).poll_read(cx, buf);
        self.captured
            .lock()
            .unwrap()
            .extend_from_slice(&buf.filled()[before..]);
        result
    }
}

impl AsyncWrite for CapturingIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

pub async fn read_request(stream: &mut (impl AsyncRead + Unpin)) -> Vec<u8> {
    let mut request = Vec::new();
    loop {
        request.push(stream.read_u8().await.unwrap());
        if request.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let headers = String::from_utf8_lossy(&request);
    let length = headers
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .map(|(_, v)| v.trim().parse::<usize>().unwrap())
        .unwrap_or(0);
    let mut body = vec![0; length];
    stream.read_exact(&mut body).await.unwrap();
    request.extend(body);
    request
}

pub async fn send_response(stream: &mut TlsStream, headers: &str, body: &[u8]) {
    stream.write_all(headers.as_bytes()).await.unwrap();
    stream.write_all(body).await.unwrap();
    stream.flush().await.unwrap();
}

pub fn normalize_hello(mut bytes: Vec<u8>) -> Vec<u8> {
    fn u16_at(bytes: &[u8], p: usize) -> usize {
        usize::from(u16::from_be_bytes([bytes[p], bytes[p + 1]]))
    }
    assert_eq!(bytes[0], 22);
    let end = 5 + u16_at(&bytes, 3);
    bytes.truncate(end);
    assert_eq!(bytes[5], 1);
    let mut p = 11;
    bytes[p..p + 32].fill(0);
    p += 32;
    let session = usize::from(bytes[p]);
    p += 1;
    bytes[p..p + session].fill(0);
    p += session;
    p += 2 + u16_at(&bytes, p);
    p += 1 + usize::from(bytes[p]);
    assert_eq!(p + 2 + u16_at(&bytes, p), end);
    p += 2;
    while p < end {
        let kind = u16_at(&bytes, p);
        let length = u16_at(&bytes, p + 2);
        p += 4;
        if kind == 51 {
            let mut k = p + 2;
            while k < p + length {
                let n = u16_at(&bytes, k + 2);
                k += 4;
                bytes[k..k + n].fill(0);
                k += n;
            }
        }
        p += length;
    }
    assert_eq!(p, end);
    bytes
}
