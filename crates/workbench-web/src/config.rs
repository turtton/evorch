//! Server configuration and its validation.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use workbench_protocol::handshake::WireLimits;
use workbench_protocol::validation::ValidationError;

/// Port used when a configuration does not name one.
pub const DEFAULT_PORT: u16 = 7878;

/// Host names that always pass the `Host` check: the loopback names a local
/// browser uses. Anything else, such as a Tailscale Serve name, is opt-in.
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Configuration of the web adapter (ADR 0045 §5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebServerConfig {
    /// Serving is opt-in. When `false`, [`WebServer::start`](crate::WebServer::start)
    /// binds nothing.
    pub enabled: bool,
    /// Must be a loopback address. There is no fallback to another address.
    pub bind: SocketAddr,
    /// Extra host names (no port, no scheme) the `Host` header may carry, for
    /// example the name Tailscale Serve publishes. The loopback names in
    /// [`LOOPBACK_HOSTS`] are always allowed. Wildcards are rejected.
    pub allowed_hosts: Vec<String>,
    /// Extra origins (`scheme://host[:port]`) that may send state-changing
    /// requests. An origin identical to the request's own `Host` is always
    /// allowed (same origin), so this is only for a different front end.
    pub allowed_origins: Vec<String>,
    /// Largest request body, in bytes.
    pub max_request_bytes: usize,
    /// Most open TCP connections; further ones are closed without a reply.
    pub max_connections: usize,
    /// Most requests handled at once; further ones get `429`.
    pub max_in_flight_requests: usize,
    /// Most open SSE streams; further ones get `429`.
    pub max_sse_streams: usize,
    /// How often an idle SSE stream sends a comment line.
    pub sse_keepalive: Duration,
    /// How long a connection may take to deliver a complete request head. It
    /// also closes keep-alive connections that stay idle, so a client cannot
    /// hold a connection slot without asking for anything.
    pub header_read_timeout: Duration,
    /// How long a request body may take to arrive in full. A body that is not
    /// complete by then is answered with `408` and the connection is closed.
    pub body_read_timeout: Duration,
    /// How long shutdown waits for open connections to finish before it drops
    /// them. A command a backend has already accepted keeps running.
    pub shutdown_grace: Duration,
    /// Per-subscription queue bounds (`subscription_queue_events` and
    /// `subscription_queue_bytes`) and the limits request decoding uses.
    pub wire_limits: WireLimits,
}

impl Default for WebServerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), DEFAULT_PORT),
            allowed_hosts: Vec::new(),
            allowed_origins: Vec::new(),
            max_request_bytes: 1024 * 1024,
            max_connections: 64,
            max_in_flight_requests: 32,
            max_sse_streams: 16,
            sse_keepalive: Duration::from_secs(15),
            header_read_timeout: Duration::from_secs(10),
            body_read_timeout: Duration::from_secs(30),
            shutdown_grace: Duration::from_secs(5),
            wire_limits: WireLimits::default(),
        }
    }
}

/// Why a configuration was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConfigError {
    #[error("web bind address {0} is not a loopback address")]
    NonLoopbackBind(SocketAddr),
    #[error("invalid allowed host `{0}`: expected a bare host name such as `host.example.ts.net`")]
    InvalidHost(String),
    #[error("invalid allowed origin `{0}`: expected `http(s)://host[:port]`")]
    InvalidOrigin(String),
    #[error("invalid limit: {0}")]
    InvalidLimit(&'static str),
    #[error("invalid wire limits: {0}")]
    WireLimits(#[from] ValidationError),
}

/// The checked, normalized form of the allowlists.
#[derive(Debug, Clone)]
pub(crate) struct Policy {
    hosts: Vec<String>,
    origins: Vec<String>,
}

impl WebServerConfig {
    /// Check the configuration without binding anything.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.policy().map(|_| ())
    }

    pub(crate) fn policy(&self) -> Result<Policy, ConfigError> {
        if !self.bind.ip().is_loopback() {
            return Err(ConfigError::NonLoopbackBind(self.bind));
        }
        if self.max_request_bytes == 0 {
            return Err(ConfigError::InvalidLimit("max_request_bytes must be > 0"));
        }
        if self.max_connections == 0 {
            return Err(ConfigError::InvalidLimit("max_connections must be > 0"));
        }
        if self.max_in_flight_requests == 0 {
            return Err(ConfigError::InvalidLimit(
                "max_in_flight_requests must be > 0",
            ));
        }
        if self.max_sse_streams == 0 {
            return Err(ConfigError::InvalidLimit("max_sse_streams must be > 0"));
        }
        for (message, value) in [
            ("sse_keepalive must be > 0", self.sse_keepalive),
            ("header_read_timeout must be > 0", self.header_read_timeout),
            ("body_read_timeout must be > 0", self.body_read_timeout),
            ("shutdown_grace must be > 0", self.shutdown_grace),
        ] {
            if value.is_zero() {
                return Err(ConfigError::InvalidLimit(message));
            }
        }
        self.wire_limits.validate()?;

        let mut hosts: Vec<String> = LOOPBACK_HOSTS.iter().map(|h| (*h).to_owned()).collect();
        for host in &self.allowed_hosts {
            let normalized =
                normalize_bare_host(host).ok_or_else(|| ConfigError::InvalidHost(host.clone()))?;
            if !hosts.contains(&normalized) {
                hosts.push(normalized);
            }
        }
        let mut origins = Vec::new();
        for origin in &self.allowed_origins {
            let normalized = normalize_origin(origin)
                .ok_or_else(|| ConfigError::InvalidOrigin(origin.clone()))?;
            if !origins.contains(&normalized) {
                origins.push(normalized);
            }
        }
        Ok(Policy { hosts, origins })
    }
}

impl Policy {
    /// Whether a `Host` header value names an allowed host (port ignored).
    pub(crate) fn host_allowed(&self, host_header: &str) -> bool {
        authority_host(host_header).is_some_and(|host| self.hosts.contains(&host))
    }

    /// Whether `origin` may send requests to a server addressed as `host_header`.
    ///
    /// `host_header` must already have passed [`Self::host_allowed`].
    pub(crate) fn origin_allowed(&self, origin: &str, host_header: &str) -> bool {
        let Some(normalized) = normalize_origin(origin) else {
            // Includes `null`, other schemes and anything with a path.
            return false;
        };
        if self.origins.contains(&normalized) {
            return true;
        }
        // Same origin: the page was served from the very authority it is
        // calling. The scheme can differ because Tailscale Serve terminates TLS.
        let origin_authority = normalized.split_once("://").map(|(_, rest)| rest);
        origin_authority == Some(host_header.to_ascii_lowercase().as_str())
    }
}

fn is_host_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')
}

fn strip_trailing_dot(host: &str) -> &str {
    host.strip_suffix('.').unwrap_or(host)
}

/// Lowercased host name of an authority (`host`, `host:port`, `[v6]`, `[v6]:port`).
///
/// Returns `None` for anything that is not plainly an authority: user info,
/// paths, whitespace, a non-numeric port.
pub(crate) fn authority_host(authority: &str) -> Option<String> {
    let authority = authority.to_ascii_lowercase();
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (inner, after) = rest.split_once(']')?;
        let ok = !inner.is_empty()
            && inner
                .chars()
                .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.');
        if !ok {
            return None;
        }
        let port = match after {
            "" => None,
            _ => Some(after.strip_prefix(':')?),
        };
        (format!("[{inner}]"), port)
    } else {
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority.as_str(), None),
        };
        let host = strip_trailing_dot(host);
        if host.is_empty() || !host.chars().all(is_host_char) {
            return None;
        }
        (host.to_owned(), port)
    };
    if let Some(port) = port
        && (port.is_empty() || port.len() > 5 || !port.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    Some(host)
}

/// A configured host: a bare name, no port.
fn normalize_bare_host(host: &str) -> Option<String> {
    if host.contains(':') && !host.starts_with('[') {
        return None;
    }
    let normalized = authority_host(host)?;
    // `authority_host` tolerates a port on bracketed hosts; configuration does not.
    if host.starts_with('[') && !host.ends_with(']') {
        return None;
    }
    Some(normalized)
}

/// `scheme://authority`, lowercased; only http and https; no path or query.
pub(crate) fn normalize_origin(origin: &str) -> Option<String> {
    let lower = origin.to_ascii_lowercase();
    let (scheme, authority) = lower.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    authority_host(authority)?;
    Some(format!("{scheme}://{authority}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authority_host_handles_ports_and_ipv6() {
        assert_eq!(
            authority_host("Localhost:8080").as_deref(),
            Some("localhost")
        );
        assert_eq!(authority_host("[::1]:8080").as_deref(), Some("[::1]"));
        assert_eq!(authority_host("a.b.ts.net.").as_deref(), Some("a.b.ts.net"));
        for bad in [
            "", "a@b", "a/b", "a b", "host:", "host:12x", "[::1", "[]", "a:1:2",
        ] {
            assert_eq!(authority_host(bad), None, "{bad}");
        }
    }

    #[test]
    fn wildcard_and_scheme_hosts_are_rejected() {
        for bad in [
            "*",
            "*.ts.net",
            "http://a.ts.net",
            "a.ts.net:443",
            "a/b",
            "",
        ] {
            let config = WebServerConfig {
                allowed_hosts: vec![bad.to_owned()],
                ..WebServerConfig::default()
            };
            assert!(
                matches!(config.validate(), Err(ConfigError::InvalidHost(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn origins_must_be_http_origins() {
        for bad in ["*", "null", "ftp://a", "https://a/path", "a.ts.net"] {
            let config = WebServerConfig {
                allowed_origins: vec![bad.to_owned()],
                ..WebServerConfig::default()
            };
            assert!(
                matches!(config.validate(), Err(ConfigError::InvalidOrigin(_))),
                "{bad}"
            );
        }
    }
}
