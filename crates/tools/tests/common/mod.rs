use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::Arc,
};

use rcgen::{CertifiedKey, generate_simple_self_signed};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinSet,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, pki_types::PrivateKeyDer},
};

pub struct FixtureServer {
    addr: SocketAddr,
    certificate: reqwest::Certificate,
}

impl FixtureServer {
    pub async fn start(
        response: impl Fn(&str) -> Vec<u8> + Send + Sync + 'static,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let _provider = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec!["fixture.test".to_owned()])?;
        let certificate = reqwest::Certificate::from_der(cert.der())?;
        let private_key = PrivateKeyDer::try_from(signing_key.serialize_der())?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![cert.into()], private_key)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let addr = listener.local_addr()?;
        let response = Arc::new(response);

        tokio::spawn(async move {
            let mut connections = JoinSet::new();
            loop {
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let acceptor = acceptor.clone();
                let response = response.clone();
                connections.spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    loop {
                        let Ok(read) = stream.read(&mut buffer).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let path = request_path(&request).unwrap_or("/");
                    let bytes = response(path);
                    let _write_result = stream.write_all(&bytes).await;
                    let _shutdown_result = stream.shutdown().await;
                });
            }
        });

        Ok(Self { addr, certificate })
    }

    pub fn url(&self, path: &str) -> String {
        format!("https://fixture.test:{}{path}", self.addr.port())
    }

    pub fn certificate(&self) -> reqwest::Certificate {
        self.certificate.clone()
    }

    pub const fn resolver_addr(&self) -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }
}

fn request_path(request: &[u8]) -> Option<&str> {
    let line_end = request.windows(2).position(|window| window == b"\r\n")?;
    let line = std::str::from_utf8(&request[..line_end]).ok()?;
    line.split_whitespace().nth(1)
}

pub fn identity_response(body: &[u8]) -> Vec<u8> {
    response_with_headers(&[format!("Content-Length: {}", body.len())], body)
}

pub fn response_with_headers(headers: &[String], body: &[u8]) -> Vec<u8> {
    let mut response = b"HTTP/1.1 200 OK\r\nConnection: close\r\n".to_vec();
    for header in headers {
        response.extend_from_slice(header.as_bytes());
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b"\r\n");
    response.extend_from_slice(body);
    response
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

pub type TestResult = Result<(), Box<dyn std::error::Error>>;
