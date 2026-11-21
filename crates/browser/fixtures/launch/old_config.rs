//! Independent baseline: AgentBrowser's pre-B3-04 config (chromiumoxide 0.9.1).
use chromiumoxide::BrowserConfig;
use std::{net::SocketAddr, path::Path, time::Duration};
pub fn config(executable: &Path, profile: &Path, proxy: SocketAddr) -> BrowserConfig {
    BrowserConfig::builder()
        .new_headless_mode()
        .chrome_executable(executable)
        .user_data_dir(profile)
        .window_size(1280, 800)
        .viewport(None)
        .respect_https_errors()
        .launch_timeout(Duration::from_secs(20))
        .request_timeout(Duration::from_secs(30))
        .arg(("proxy-server", format!("http://{proxy}").as_str()))
        .args([
            "no-first-run",
            "no-default-browser-check",
            "disable-background-networking",
            "disable-component-update",
            "disable-sync",
            "disable-extensions",
            "mute-audio",
            "disable-quic",
        ])
        .args([
            ("disable-features", "IsolateOrigins,site-per-process"),
            ("disable-features", "BackForwardCache"),
            ("proxy-bypass-list", "<-loopback>"),
            ("force-webrtc-ip-handling-policy", "disable_non_proxied_udp"),
        ])
        .build()
        .unwrap()
}
