//! MCP-specific admission; no exception is stored on NetworkGuard itself.
use super::NetworkGuard;
use crate::mcp::{
    McpErrorKind,
    types::{ApprovalFingerprint, ApprovalPermit, ApprovalScope},
};
use std::{
    fmt,
    net::{IpAddr, SocketAddr},
};
use url::Url;
/// Opaque, non-serializable, exact endpoint + server/project + approved IP binding.
pub struct ApprovedMcpDestination {
    pub(super) endpoint: Url,
    pub(super) ips: Vec<IpAddr>,
    scope: ApprovalScope,
    fingerprint: ApprovalFingerprint,
}
impl fmt::Debug for ApprovedMcpDestination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ApprovedMcpDestination(<opaque>)")
    }
}
#[derive(Debug)]
pub struct McpDestinationInspection {
    pub ips: Vec<IpAddr>,
    pub requires_approval: bool,
}
fn normalized(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v) => v.to_ipv4_mapped().map_or(ip, IpAddr::V4),
        _ => ip,
    }
}
fn local(ip: IpAddr) -> bool {
    match normalized(ip) {
        IpAddr::V4(v) => v.is_private() || v.is_loopback(),
        IpAddr::V6(v) => v.is_loopback() || v.is_unique_local(),
    }
}
fn forbidden(ip: IpAddr) -> bool {
    match normalized(ip) {
        IpAddr::V4(v) => {
            v.is_link_local()
                || v.is_multicast()
                || v.is_unspecified()
                || v.is_broadcast()
                || v.octets()[0] == 0
        }
        IpAddr::V6(v) => v.is_unicast_link_local() || v.is_multicast() || v.is_unspecified(),
    }
}
fn endpoint(value: &str) -> Result<Url, McpErrorKind> {
    let url = Url::parse(value).map_err(|_| McpErrorKind::Configuration)?;
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
        || url.port_or_known_default().is_none_or(|p| p == 0)
    {
        return Err(McpErrorKind::Configuration);
    }
    Ok(url)
}
impl NetworkGuard {
    async fn mcp_ips(&self, url: &Url) -> Result<Vec<IpAddr>, McpErrorKind> {
        let host = url
            .host_str()
            .ok_or(McpErrorKind::Configuration)?
            .trim_start_matches('[')
            .trim_end_matches(']');
        let ips = match host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => self
                .resolver
                .resolve(host)
                .await
                .map_err(|_| McpErrorKind::Transport)?,
        };
        if ips.is_empty() {
            return Err(McpErrorKind::Transport);
        }
        if ips
            .iter()
            .any(|ip| forbidden(*ip) || self.policy.is_blocked(*ip))
        {
            return Err(McpErrorKind::Transport);
        }
        Ok(ips)
    }
    /// DNS-only inspection for approval UI, never sends credentials/connects.
    pub async fn inspect_mcp_destination(
        &self,
        value: &str,
    ) -> Result<McpDestinationInspection, McpErrorKind> {
        let url = endpoint(value)?;
        let ips = self.mcp_ips(&url).await?;
        // Plaintext is a localhost/LAN exception, never a public-HTTP grant.
        // Reject mixed DNS answers too; approving every answer cannot widen it.
        if url.scheme() == "http" && ips.iter().any(|ip| !local(*ip)) {
            return Err(McpErrorKind::Transport);
        }
        let requires_approval = url.scheme() == "http" || ips.iter().any(|ip| local(*ip));
        Ok(McpDestinationInspection {
            ips,
            requires_approval,
        })
    }
    /// Only a fresh live approval permit can turn an IP inspection into admission.
    pub async fn approve_mcp_destination(
        &self,
        value: &str,
        permit: &ApprovalPermit,
        approved_ips: &[IpAddr],
    ) -> Result<ApprovedMcpDestination, McpErrorKind> {
        if permit.is_revoked() {
            return Err(McpErrorKind::ApprovalRevoked);
        }
        let url = endpoint(value)?;
        let inspection = self.inspect_mcp_destination(value).await?;
        if inspection.requires_approval
            && (approved_ips.is_empty()
                || inspection.ips.iter().any(|ip| !approved_ips.contains(ip)))
        {
            return Err(McpErrorKind::NeedsApproval);
        }
        Ok(ApprovedMcpDestination {
            endpoint: url,
            ips: inspection.ips,
            scope: permit.scope().clone(),
            fingerprint: permit.fingerprint(),
        })
    }
    /// Re-resolve just before sending and pin the approved answer; no second DNS lookup.
    pub async fn check_mcp_destination(
        &self,
        value: &str,
        destination: &ApprovedMcpDestination,
        permit: &ApprovalPermit,
    ) -> Result<Vec<SocketAddr>, McpErrorKind> {
        if permit.is_revoked() {
            return Err(McpErrorKind::ApprovalRevoked);
        }
        if permit.scope() != &destination.scope
            || permit.fingerprint() != destination.fingerprint
            || endpoint(value)? != destination.endpoint
        {
            return Err(McpErrorKind::NeedsApproval);
        }
        let ips = self.mcp_ips(&destination.endpoint).await?;
        if ips.iter().any(|ip| !destination.ips.contains(ip)) {
            return Err(McpErrorKind::NeedsApproval);
        }
        Ok(ips
            .into_iter()
            .map(|ip| {
                SocketAddr::new(
                    ip,
                    destination
                        .endpoint
                        .port_or_known_default()
                        .expect("checked port"),
                )
            })
            .collect())
    }
}
