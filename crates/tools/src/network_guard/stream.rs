//! Bounded MCP-only streaming boundary. Dropping the response closes its stream.
use super::{MAX_RESPONSE_BYTES, NetworkGuard, mcp::ApprovedMcpDestination};
use crate::mcp::{
    McpErrorKind,
    types::{ApprovalPermit, MAX_REQUEST_BYTES, MAX_RESPONSE_HEADER_BYTES},
};
use reqwest::{
    StatusCode,
    header::{CONTENT_ENCODING, CONTENT_LENGTH, HeaderMap},
};
use serde_json::Value;
use std::time::Duration;

pub struct CheckedMcpStream {
    response: reqwest::Response,
    received: usize,
}
impl CheckedMcpStream {
    pub fn status(&self) -> StatusCode {
        self.response.status()
    }
    pub fn headers(&self) -> &HeaderMap {
        self.response.headers()
    }
    pub async fn chunk(&mut self) -> Result<Option<Vec<u8>>, McpErrorKind> {
        let chunk = self.response.chunk().await.map_err(classify)?;
        if let Some(chunk) = chunk {
            self.received = self
                .received
                .checked_add(chunk.len())
                .ok_or(McpErrorKind::LimitExceeded)?;
            if self.received > MAX_RESPONSE_BYTES {
                return Err(McpErrorKind::LimitExceeded);
            }
            Ok(Some(chunk.to_vec()))
        } else {
            Ok(None)
        }
    }
}
pub(crate) fn classify(error: reqwest::Error) -> McpErrorKind {
    if error.is_timeout() {
        McpErrorKind::Timeout
    } else {
        McpErrorKind::Transport
    }
}
impl NetworkGuard {
    /// Applies the MCP server's deadline to response headers and streamed body,
    /// independently of the generic web guard's request timeout.
    pub async fn post_mcp_stream(
        &self,
        value: &str,
        destination: &ApprovedMcpDestination,
        permit: &ApprovalPermit,
        headers: HeaderMap,
        body: &Value,
        request_timeout: Duration,
    ) -> Result<CheckedMcpStream, McpErrorKind> {
        let bytes = serde_json::to_vec(body).map_err(|_| McpErrorKind::Configuration)?;
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        let addresses = self
            .check_mcp_destination(value, destination, permit)
            .await?;
        let host = destination
            .endpoint
            .host_str()
            .ok_or(McpErrorKind::Configuration)?
            .trim_start_matches('[')
            .trim_end_matches(']');
        // A fresh client per checked request prevents pool reuse from skipping admission.
        let mut builder = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .danger_accept_invalid_certs(false)
            .connect_timeout(self.connect_timeout)
            .timeout(request_timeout)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .resolve_to_addrs(host, &addresses);
        if let Some(certificate) = self.root_certificate.clone() {
            builder = builder.add_root_certificate(certificate);
        }
        let client = builder.build().map_err(classify)?;
        if permit.is_revoked() {
            return Err(McpErrorKind::ApprovalRevoked);
        }
        let response = client
            .post(destination.endpoint.clone())
            .headers(headers)
            .json(body)
            .send()
            .await
            .map_err(classify)?;
        if response.status().is_redirection() {
            return Err(McpErrorKind::Transport);
        }
        let header_bytes: usize = response
            .headers()
            .iter()
            .map(|(n, v)| n.as_str().len() + v.as_bytes().len())
            .sum();
        if header_bytes > MAX_RESPONSE_HEADER_BYTES {
            return Err(McpErrorKind::LimitExceeded);
        }
        if response.headers().get(CONTENT_LENGTH).is_some_and(|v| {
            v.to_str()
                .ok()
                .and_then(|s| s.parse::<usize>().ok())
                .is_none_or(|n| n > MAX_RESPONSE_BYTES)
        }) {
            return Err(McpErrorKind::LimitExceeded);
        }
        // Ask for identity only and reject unsolicited compression, rather than expose
        // a decompressor whose expansion/stream termination could evade the byte cap.
        if response
            .headers()
            .get(CONTENT_ENCODING)
            .is_some_and(|v| v != "identity")
        {
            return Err(McpErrorKind::Protocol);
        }
        Ok(CheckedMcpStream {
            response,
            received: 0,
        })
    }
}
