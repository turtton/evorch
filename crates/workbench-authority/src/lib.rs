//! Transport proof is not wire provenance. The only local issuer verifies
//! kernel credentials. A future Web verifier must live inside this crate;
//! currently its opaque request-context type has no production issuer.
//!
//! ```
//! use workbench_authority::IngressContext;
//! use workbench_protocol::{envelope::{Provenance, Surface}, ids::ClientId};
//! let context = IngressContext::unverified(Provenance::human(Surface::Web, ClientId::new("tab").unwrap()));
//! assert!(context.authorize().is_err());
//! ```
//! ```compile_fail
//! let _: workbench_authority::TransportAuthority = serde_json::from_str("{}").unwrap();
//! ```
//! ```compile_fail
//! let _ = workbench_authority::VerifiedWebPrincipal { subject: "admin".into() };
//! ```
//! ```compile_fail
//! let _ = workbench_authority::TransportAuthority::verified("admin");
//! ```
use std::{fmt, sync::Arc};
use tokio::sync::watch;
use workbench_protocol::envelope::{Actor, Provenance, Surface};

/// Credential-free bounded audit identity; never a public view.
#[derive(Clone, PartialEq, Eq)]
pub struct PrincipalRecord {
    method: String,
    subject: String,
    binding: String,
}
impl fmt::Debug for PrincipalRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PrincipalRecord([redacted])")
    }
}
impl PrincipalRecord {
    pub fn method(&self) -> &str {
        &self.method
    }
    pub fn subject(&self) -> &str {
        &self.subject
    }
    pub fn binding(&self) -> &str {
        &self.binding
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    Local,
    #[allow(dead_code)] // Web verification is deliberately not implemented.
    Web,
}
struct Proof {
    source: Source,
    principal: PrincipalRecord,
    live: watch::Receiver<bool>,
    deadline: Option<tokio::time::Instant>,
}
impl Proof {
    fn is_live(&self) -> bool {
        *self.live.borrow()
            && self.live.has_changed().is_ok()
            && self
                .deadline
                .is_none_or(|deadline| tokio::time::Instant::now() < deadline)
    }
}
/// Default is unauthenticated. No serde, literal or claimed-identity issuer.
#[derive(Clone, Default)]
pub struct TransportAuthority(Option<Arc<Proof>>);
impl fmt::Debug for TransportAuthority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TransportAuthority([opaque])")
    }
}
impl TransportAuthority {
    pub fn from_web_context(principal: Option<&VerifiedWebPrincipal>) -> Self {
        principal.map_or_else(Self::default, |p| Self(Some(p.0.clone())))
    }
    pub fn principal(&self) -> Option<&PrincipalRecord> {
        self.0
            .as_ref()
            .filter(|p| p.is_live())
            .map(|p| &p.principal)
    }
    pub fn is_verified_local(&self) -> bool {
        self.0
            .as_ref()
            .is_some_and(|p| p.source == Source::Local && p.is_live())
    }
    /// Ends on revocation or absolute expiry. Anonymous contexts do not expire.
    pub async fn expired(&self) {
        let Some(proof) = &self.0 else {
            return std::future::pending().await;
        };
        let mut live = proof.live.clone();
        loop {
            if !proof.is_live() {
                return;
            }
            match proof.deadline {
                Some(deadline) => tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => return,
                    result = live.changed() => if result.is_err() { return; },
                },
                None => {
                    if live.changed().await.is_err() {
                        return;
                    }
                }
            }
        }
    }
}
/// Only a verifier-owned request extension can provide this. No current issuer.
#[derive(Clone)]
pub struct VerifiedWebPrincipal(Arc<Proof>);
/// Request-scoped; saving Provenance never saves proof.
#[derive(Debug, Clone)]
pub struct IngressContext {
    provenance: Provenance,
    authority: TransportAuthority,
    system: bool,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AuthorityDenial {
    Unauthenticated,
    NonHuman,
    SurfaceMismatch,
    Expired,
}
impl IngressContext {
    pub fn unverified(provenance: Provenance) -> Self {
        Self {
            provenance,
            authority: TransportAuthority::default(),
            system: false,
        }
    }
    pub fn new(provenance: Provenance, authority: TransportAuthority) -> Self {
        Self {
            provenance,
            authority,
            system: false,
        }
    }
    /// Automatic paths stay unverified even when their bookkeeping says Human.
    pub fn system(provenance: Provenance) -> Self {
        Self {
            system: true,
            ..Self::unverified(provenance)
        }
    }
    pub fn provenance(&self) -> &Provenance {
        &self.provenance
    }
    pub fn authority(&self) -> &TransportAuthority {
        &self.authority
    }
    pub fn authorize(&self) -> Result<&PrincipalRecord, AuthorityDenial> {
        if self.system || self.provenance.actor != Actor::Human {
            return Err(AuthorityDenial::NonHuman);
        }
        if self.provenance.validate().is_err() {
            return Err(AuthorityDenial::SurfaceMismatch);
        }
        let proof = self
            .authority
            .0
            .as_ref()
            .ok_or(AuthorityDenial::Unauthenticated)?;
        if !proof.is_live() {
            return Err(AuthorityDenial::Expired);
        }
        if !matches!(
            (proof.source, self.provenance.surface),
            (Source::Local, Surface::Desktop | Surface::Cli) | (Source::Web, Surface::Web)
        ) {
            return Err(AuthorityDenial::SurfaceMismatch);
        }
        Ok(&proof.principal)
    }
}
impl From<Provenance> for IngressContext {
    fn from(provenance: Provenance) -> Self {
        Self::unverified(provenance)
    }
}

/// A local proof lives exactly as long as this verified live connection.
/// Dropping the guard revokes every request-scoped clone.
pub struct LocalConnection {
    authority: TransportAuthority,
    _lease: watch::Sender<bool>,
}
impl LocalConnection {
    pub fn authority(&self) -> TransportAuthority {
        self.authority.clone()
    }
}
/// Verify the actual socket, never a UID supplied by a client. Product callers
/// are restricted to evorch-core's Unix ingress by the boundary gate.
#[cfg(unix)]
pub fn verify_unix_peer(stream: &tokio::net::UnixStream) -> std::io::Result<LocalConnection> {
    let uid = stream.peer_cred()?.uid();
    if uid != rustix::process::geteuid().as_raw() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "wrong socket peer owner",
        ));
    }
    let (lease, live) = watch::channel(true);
    let proof = Proof {
        source: Source::Local,
        principal: PrincipalRecord {
            method: "unix-peer-uid".into(),
            subject: uid.to_string(),
            binding: uuid::Uuid::new_v4().to_string(),
        },
        live,
        deadline: None,
    };
    Ok(LocalConnection {
        authority: TransportAuthority(Some(Arc::new(proof))),
        _lease: lease,
    })
}
/// Contract-only issuers: never enable via a product feature.
#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::*;
    pub struct Lease(watch::Sender<bool>);
    impl Lease {
        pub fn revoke(&self) {
            self.0.send_replace(false);
        }
    }
    fn issue(
        source: Source,
        subject: &str,
        deadline: Option<tokio::time::Instant>,
    ) -> (Arc<Proof>, Lease) {
        assert!(
            !subject.is_empty() && subject.len() <= 256 && !subject.chars().any(char::is_control)
        );
        let (sender, live) = watch::channel(true);
        (
            Arc::new(Proof {
                source,
                principal: PrincipalRecord {
                    method: "contract-fake".into(),
                    subject: subject.into(),
                    binding: uuid::Uuid::new_v4().to_string(),
                },
                live,
                deadline,
            }),
            Lease(sender),
        )
    }
    pub fn local() -> (TransportAuthority, Lease) {
        let (proof, lease) = issue(Source::Local, "local-contract", None);
        (TransportAuthority(Some(proof)), lease)
    }
    pub fn web(subject: &str, deadline: tokio::time::Instant) -> (VerifiedWebPrincipal, Lease) {
        let (proof, lease) = issue(Source::Web, subject, Some(deadline));
        (VerifiedWebPrincipal(proof), lease)
    }
}
#[cfg(test)]
mod tests;
