//! A loopback HTTPS fixture whose CA is trusted only through the cell's own
//! temporary NSS database. The core's NetworkGuard allows loopback; Chromium
//! (not the product) verifies the certificate.
use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose, SanType,
};
use std::{
    io,
    net::Ipv4Addr,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_rustls::{
    TlsAcceptor,
    rustls::{ServerConfig, pki_types::PrivateKeyDer},
};

pub const HEADING: &str = "B3-08 navigation fixture";

pub struct Fixture {
    pub port: u16,
    pub ca_pem: String,
    /// First line of each request that completed a TLS handshake and was read.
    pub requests: Arc<Mutex<Vec<String>>>,
    serving: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.serving.abort();
    }
}

fn other(error: impl std::fmt::Display) -> io::Error {
    io::Error::other(error.to_string())
}

impl Fixture {
    pub async fn start() -> io::Result<Self> {
        let _ = tokio_rustls::rustls::crypto::ring::default_provider().install_default();
        let ca_key = KeyPair::generate().map_err(other)?;
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).map_err(other)?;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params
            .distinguished_name
            .push(DnType::CommonName, "evorch B3-08 test CA");
        let ca = ca_params.self_signed(&ca_key).map_err(other)?;
        let issuer = Issuer::new(ca_params, ca_key);
        let leaf_key = KeyPair::generate().map_err(other)?;
        let mut leaf_params = CertificateParams::new(Vec::<String>::new()).map_err(other)?;
        leaf_params.subject_alt_names = vec![SanType::IpAddress(Ipv4Addr::LOCALHOST.into())];
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        leaf_params
            .distinguished_name
            .push(DnType::CommonName, "127.0.0.1");
        let leaf = leaf_params.signed_by(&leaf_key, &issuer).map_err(other)?;
        let key = PrivateKeyDer::try_from(leaf_key.serialize_der()).map_err(other)?;
        let config = ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![leaf.into()], key)
            .map_err(other)?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let log = requests.clone();
        let serving = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let (acceptor, log) = (acceptor.clone(), log.clone());
                tokio::spawn(async move {
                    let Ok(mut stream) = acceptor.accept(stream).await else {
                        return;
                    };
                    let mut request = Vec::new();
                    let mut buffer = [0_u8; 1024];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(n) => request.extend_from_slice(&buffer[..n]),
                        }
                    }
                    let line = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().unwrap().push(line);
                    let page = format!(
                        "<!doctype html><html><head><title>b3</title></head><body><h1>{HEADING}</h1></body></html>"
                    );
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                        page.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        Ok(Self {
            port,
            ca_pem: ca.pem(),
            requests,
            serving,
        })
    }
    pub fn url(&self) -> String {
        format!("https://127.0.0.1:{}/b3-08", self.port)
    }
}
