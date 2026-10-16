//! Claude Code 2.1.295's captured TLS and HTTP/1 transport profile.

use std::time::Duration;

use futures_util::StreamExt;
use reqwest::header::HeaderMap;
use serde_json::Value;
use wreq::header::OrigHeaderMap;
use wreq::tls::{
    AlpnProtocol, ExtensionType, KeyShare, TlsOptions, TlsVersion,
    trust::{CertStore, Certificate},
};

use crate::{ProviderAuth, ProviderError};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const READ_TIMEOUT: Duration = Duration::from_secs(90);
const JSON_LIMIT: usize = 1024 * 1024;
const CIPHERS: &str = concat!(
    "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:",
    "ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES128-GCM-SHA256:",
    "ECDHE-ECDSA-AES256-GCM-SHA384:ECDHE-RSA-AES256-GCM-SHA384:",
    "ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-CHACHA20-POLY1305:",
    "ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES128-SHA:",
    "ECDHE-ECDSA-AES256-SHA:ECDHE-RSA-AES256-SHA:",
    "AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA:AES256-SHA"
);

#[derive(Clone)]
pub(crate) struct ClaudeTransport {
    client: wreq::Client,
}

impl ClaudeTransport {
    pub fn new() -> Result<Self, ProviderError> {
        let mut builder = client_builder();
        if let Some(path) = std::env::var_os("NODE_EXTRA_CA_CERTS") {
            let pem = std::fs::read(path).map_err(|_| invalid_ca())?;
            builder = builder.tls_cert_store(additional_roots(&pem)?);
        }
        Ok(Self {
            client: builder.build().map_err(map_transport_error)?,
        })
    }

    pub fn request(
        &self,
        method: wreq::Method,
        url: &str,
        auth: &ProviderAuth,
        headers: HeaderMap,
    ) -> wreq::RequestBuilder {
        self.client
            .request(method, url)
            .headers(headers)
            .bearer_auth(&auth.api_key)
    }
}

fn additional_roots(pem: &[u8]) -> Result<CertStore, ProviderError> {
    let certificates = Certificate::stack_from_pem(pem).map_err(|_| invalid_ca())?;
    if certificates.is_empty() {
        return Err(invalid_ca());
    }
    CertStore::builder()
        .add_der_certs(webpki_root_certs::TLS_SERVER_ROOT_CERTS)
        .add_der_certs(certificates)
        .build()
        .map_err(|_| invalid_ca())
}

fn client_builder() -> wreq::ClientBuilder {
    wreq::Client::builder()
        .tls_options(tls_options())
        .http1_only()
        .orig_headers(original_headers())
        .tls_cert_verification(true)
        .tls_verify_hostname(true)
        .redirect(wreq::redirect::Policy::none())
        .retry(wreq::retry::Policy::never())
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .zstd(true)
}

fn tls_options() -> TlsOptions {
    TlsOptions::builder()
        .alpn_protocols([AlpnProtocol::HTTP1])
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .cipher_list(CIPHERS)
        .preserve_tls13_cipher_list(true)
        .curves_list("X25519MLKEM768:X25519:P-256:P-384")
        .key_shares(vec![KeyShare::X25519_MLKEM768, KeyShare::X25519])
        .sigalgs_list(concat!(
            "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:",
            "ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:",
            "rsa_pss_rsae_sha512:rsa_pkcs1_sha512:rsa_pkcs1_sha1"
        ))
        .extension_permutation(
            [0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43]
                .into_iter()
                .map(ExtensionType::from)
                .collect::<Vec<_>>(),
        )
        .enable_ocsp_stapling(true)
        .enable_signed_cert_timestamps(true)
        .grease_enabled(false)
        .permute_extensions(false)
        .build()
}

fn original_headers() -> OrigHeaderMap {
    let mut headers = OrigHeaderMap::new();
    for name in [
        "Accept",
        "Authorization",
        "Content-Type",
        "User-Agent",
        "X-Claude-Code-Session-Id",
        "X-Stainless-Arch",
        "X-Stainless-Lang",
        "X-Stainless-OS",
        "X-Stainless-Package-Version",
        "X-Stainless-Retry-Count",
        "X-Stainless-Runtime",
        "X-Stainless-Runtime-Version",
        "X-Stainless-Timeout",
        "anthropic-beta",
        "anthropic-dangerous-direct-browser-access",
        "anthropic-version",
        "x-app",
        "x-claude-code-agent-id",
        "x-claude-code-parent-agent-id",
        "x-claude-code-prev-tool-durations",
        "x-claude-code-prompt-id",
        "x-claude-code-request-class",
        "x-client-request-id",
        "Connection",
        "Host",
        "Accept-Encoding",
        "Content-Length",
    ] {
        headers.insert(name);
    }
    headers
}

fn invalid_ca() -> ProviderError {
    ProviderError::Request("Claude additional CA certificates could not be loaded".into())
}

pub(crate) fn map_transport_error(error: wreq::Error) -> ProviderError {
    if error.is_timeout() {
        ProviderError::Timeout
    } else {
        ProviderError::Transport {
            message: "Claude transport failed".into(),
        }
    }
}

pub(crate) async fn bounded_json(response: wreq::Response) -> Result<Value, ProviderError> {
    let mut stream = response.bytes_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(map_transport_error)?;
        if bytes.len().saturating_add(chunk.len()) > JSON_LIMIT {
            return Err(invalid_json());
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| invalid_json())
}

fn invalid_json() -> ProviderError {
    ProviderError::InvalidJson {
        detail: "invalid Claude response JSON".into(),
    }
}

pub(crate) async fn response_error(response: wreq::Response) -> ProviderError {
    let status = response.status().as_u16();
    if status == 429 {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok())
            .map(Duration::from_secs);
        ProviderError::RateLimited { retry_after }
    } else {
        let body = bounded_json(response)
            .await
            .ok()
            .and_then(|value| super::required_version_error(&value))
            .unwrap_or_else(|| "Claude request rejected".into());
        ProviderError::Http { status, body }
    }
}

#[cfg(test)]
mod tests;
