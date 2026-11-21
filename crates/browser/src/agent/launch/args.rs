//! The agent's fixed chromiumoxide 0.9.1 launch adapter. This intentionally
//! includes upstream DEFAULT_ARGS: the execution golden detects upstream drift.
use std::{net::SocketAddr, path::Path};

pub(super) fn arguments(profile: &Path, proxy: SocketAddr) -> Vec<String> {
    let mut args: Vec<String> = [
        "--disable-background-networking",
        "--enable-features=NetworkService,NetworkServiceInProcess",
        "--disable-background-timer-throttling",
        "--disable-backgrounding-occluded-windows",
        "--disable-breakpad",
        "--disable-client-side-phishing-detection",
        "--disable-component-extensions-with-background-pages",
        "--disable-default-apps",
        "--disable-dev-shm-usage",
        "--disable-features=TranslateUI,IsolateOrigins,site-per-process,BackForwardCache",
        "--disable-hang-monitor",
        "--disable-ipc-flooding-protection",
        "--disable-popup-blocking",
        "--disable-prompt-on-repost",
        "--disable-renderer-backgrounding",
        "--disable-sync",
        "--force-color-profile=srgb",
        "--metrics-recording-only",
        "--no-first-run",
        "--enable-automation",
        "--password-store=basic",
        "--use-mock-keychain",
        "--enable-blink-features=IdleDetection",
        "--lang=en_US",
        "--remote-debugging-port=0",
        "--disable-extensions",
        "--headless=new",
        "--hide-scrollbars",
        "--mute-audio",
        "--no-default-browser-check",
        "--disable-component-update",
        "--disable-quic",
        "--proxy-bypass-list=<-loopback>",
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(format!("--user-data-dir={}", profile.display()));
    args.push(format!(
        "--window-size={},{}",
        super::super::WINDOW[0],
        super::super::WINDOW[1]
    ));
    args.push(format!("--proxy-server=http://{proxy}"));
    args
}
