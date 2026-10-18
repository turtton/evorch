//! Network isolation for artifact previews. Each layer covers traffic the
//! others cannot see:
//!
//! 1. the artifact is served from a fictitious origin by a scheme handler;
//! 2. every other subresource and navigation is cancelled;
//! 3. a Content-Security-Policy header denies fetches the page could start;
//! 4. process-wide switches send what bypasses request hooks (WebSocket,
//!    preconnect, DNS prefetch) to an unreachable proxy, because CEF 154
//!    ignores per-request-context proxy and WebRTC preferences;
//! 5. WebRTC constructors are deleted in every renderer V8 context.

use std::collections::HashMap;
use std::sync::Arc;

pub const ARTIFACT_HOST: &str = "artifact.evorch.invalid";
pub const ARTIFACT_ORIGIN: &str = "https://artifact.evorch.invalid";
pub const INDEX: &str = "index.html";

pub const CONTENT_SECURITY_POLICY: &str = "default-src 'self' data: blob:; \
script-src 'self' 'unsafe-inline'; style-src 'self' 'unsafe-inline'; \
connect-src 'none'; frame-src 'none'; worker-src 'none'; object-src 'none'; \
form-action 'none'; base-uri 'none'";

/// Renderer switch that makes the render process handler delete WebRTC globals.
pub const DISABLE_WEBRTC_SWITCH: &str = "evorch-disable-webrtc";
pub const WEBRTC_GLOBALS: &[&str] = &[
    "RTCPeerConnection",
    "webkitRTCPeerConnection",
    "RTCDataChannel",
    "RTCIceTransport",
];

const OFFLINE_SWITCHES: &[&str] = &[
    "proxy-server=http://127.0.0.1:9",
    // `<-loopback>` cancels the implicit localhost bypass of the proxy.
    "proxy-bypass-list=<-loopback>",
    "host-resolver-rules=MAP * ~NOTFOUND",
    DISABLE_WEBRTC_SWITCH,
];

const BASE_SWITCHES: &[&str] = &[
    // Never touch the desktop keyring, which would prompt to unlock it.
    "password-store=basic",
    "disable-background-networking",
    "disable-component-update",
    "no-first-run",
    // Off-screen rendering needs no display; auto-detection would follow
    // XDG_SESSION_TYPE and abort when that compositor is unreachable.
    "ozone-platform=headless",
    "disable-gpu",
    "disable-gpu-compositing",
];

/// Browser-process switches, as `name` or `name=value` without leading dashes.
pub fn browser_switches() -> Vec<String> {
    BASE_SWITCHES
        .iter()
        .chain(OFFLINE_SWITCHES)
        .map(|switch| (*switch).to_owned())
        .collect()
}

/// Whether the page may load `url`: only the artifact origin and inline data.
pub fn is_allowed_url(url: &str) -> bool {
    url.strip_prefix(ARTIFACT_ORIGIN)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
        || url.starts_with("data:")
        || url.starts_with("blob:")
        || url == "about:blank"
}

/// Bundle path that an artifact-origin URL names, ignoring query and fragment.
pub fn artifact_path(url: &str) -> Option<&str> {
    let rest = url.strip_prefix(ARTIFACT_ORIGIN)?;
    if !(rest.is_empty() || rest.starts_with('/')) {
        return None;
    }
    let path = rest
        .split(['?', '#'])
        .next()
        .unwrap_or_default()
        .trim_start_matches('/');
    Some(if path.is_empty() { INDEX } else { path })
}

/// In-memory files served under [`ARTIFACT_ORIGIN`].
#[derive(Clone, Default)]
pub struct ArtifactBundle {
    files: HashMap<String, (String, Arc<Vec<u8>>)>,
}

impl ArtifactBundle {
    pub fn html(body: impl Into<Vec<u8>>) -> Self {
        let mut bundle = Self::default();
        bundle.insert(INDEX, "text/html", body);
        bundle
    }

    pub fn insert(&mut self, path: &str, mime: &str, body: impl Into<Vec<u8>>) {
        self.files.insert(
            path.trim_start_matches('/').to_owned(),
            (mime.to_owned(), Arc::new(body.into())),
        );
    }

    pub fn get(&self, path: &str) -> Option<(&str, Arc<Vec<u8>>)> {
        self.files
            .get(path)
            .map(|(mime, body)| (mime.as_str(), Arc::clone(body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_artifact_origin_and_inline_data_are_allowed() {
        for allowed in [
            "https://artifact.evorch.invalid",
            "https://artifact.evorch.invalid/index.html?x#y",
            "data:image/png;base64,AAAA",
            "blob:https://artifact.evorch.invalid/1234",
            "about:blank",
        ] {
            assert!(is_allowed_url(allowed), "{allowed}");
        }
        for denied in [
            "https://artifact.evorch.invalid.example.com/",
            "https://artifact.evorch.invalid:8443/",
            "http://artifact.evorch.invalid/",
            "http://127.0.0.1:9/",
            "file:///etc/hostname",
            "ws://127.0.0.1/",
            "about:srcdoc",
        ] {
            assert!(!is_allowed_url(denied), "{denied}");
        }
    }

    #[test]
    fn artifact_paths_default_to_the_index_and_drop_query_and_fragment() {
        assert_eq!(
            artifact_path("https://artifact.evorch.invalid"),
            Some(INDEX)
        );
        assert_eq!(
            artifact_path("https://artifact.evorch.invalid/?q"),
            Some(INDEX)
        );
        assert_eq!(
            artifact_path("https://artifact.evorch.invalid/img/a.png#top"),
            Some("img/a.png")
        );
        assert_eq!(
            artifact_path("https://artifact.evorch.invalid.example/"),
            None
        );
    }

    #[test]
    fn switches_keep_rendering_headless_and_route_bypassing_traffic_to_a_dead_proxy() {
        let switches = browser_switches();

        for expected in [
            "ozone-platform=headless",
            "disable-gpu",
            "proxy-server=http://127.0.0.1:9",
            "proxy-bypass-list=<-loopback>",
            DISABLE_WEBRTC_SWITCH,
        ] {
            assert!(
                switches.iter().any(|switch| switch == expected),
                "{expected}"
            );
        }
        assert!(switches.iter().all(|switch| !switch.starts_with('-')));
    }
}
