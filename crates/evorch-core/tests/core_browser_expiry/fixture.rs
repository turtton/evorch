//! Worker side (the owner process): real browser engines started through the
//! product's own owner APIs, with a navigation that is complete when the page
//! has been read back. The worker prints `NAVIGATED ...` only after that read.
//! The children are outside the host ledger, so the stalled cleanup never
//! reaches them; only the guard can reap them after the worker's `_exit(74)`.
use super::composition::FixtureFuture;
use std::{any::Any, time::Duration};

pub const CHROMIUM_HEADING: &str = crate::tls::HEADING;
pub const CEF_HEADING: &str = "B3-08b CEF fixture";
/// The CEF page's script touches this much memory before it writes the
/// heading, so an outside observer can see that the script ran.
pub const CEF_SCRIPT_MIB: usize = 384;
/// Failure bound of the worker's own start-up; the controller has its own.
const START: Duration = Duration::from_secs(90);

pub fn start(say: fn(&str)) -> FixtureFuture {
    Box::pin(async move {
        let engine = std::env::var("EVORCH_EXPIRY_BROWSER").map_err(|e| e.to_string())?;
        let held = tokio::time::timeout(START, async {
            match engine.as_str() {
                "chromium" => chromium(say).await,
                "cef" => cef(say).await,
                other => Err(format!("unknown engine {other}")),
            }
        })
        .await
        .map_err(|_| "browser navigation did not finish in time".to_owned())??;
        Ok(held)
    })
}

/// Only the cell's own loopback fixture may be reached.
struct Loopback;
#[async_trait::async_trait]
impl browser::agent::RequestPolicy for Loopback {
    async fn check(&self, url: &reqwest::Url) -> Result<(), String> {
        if url.host_str() == Some("127.0.0.1") {
            Ok(())
        } else {
            Err(format!("refused {url}"))
        }
    }
    async fn resolve(
        &self,
        host: &str,
        port: u16,
        _tls: bool,
    ) -> Result<Vec<std::net::SocketAddr>, String> {
        if host == "127.0.0.1" {
            Ok(vec![(std::net::Ipv4Addr::LOCALHOST, port).into()])
        } else {
            Err(format!("refused {host}"))
        }
    }
}

async fn chromium(say: fn(&str)) -> Result<Box<dyn Any + Send>, String> {
    use browser::agent::{AgentBrowser, LaunchOptions, PageProvider};
    let url = std::env::var("EVORCH_EXPIRY_BROWSER_URL").map_err(|e| e.to_string())?;
    let executable = std::env::var_os("CHROME").map(std::path::PathBuf::from);
    let browser = AgentBrowser::new(LaunchOptions {
        executable,
        policy: std::sync::Arc::new(Loopback),
        max_pages: 1,
        max_tabs: 1,
    });
    let page = browser.page("expiry").await.map_err(|e| e.to_string())?;
    page.navigate(&url).await.map_err(|e| e.to_string())?;
    let text = page.snapshot().await.map_err(|e| e.to_string())?.text;
    if !text.contains(CHROMIUM_HEADING) {
        return Err(format!("page was not loaded: {text:?}"));
    }
    say(&format!("NAVIGATED chromium {CHROMIUM_HEADING}"));
    Ok(Box::new((browser, page)))
}

async fn cef(say: fn(&str)) -> Result<Box<dyn Any + Send>, String> {
    use runtime::artifacts::{ArtifactPages as _, ArtifactRecord};
    use workbench_host::areas::{
        AreaTasks,
        browser::{
            renderer::{OwnerConfig, RendererOwner},
            run_pages::RuntimePages,
        },
    };
    let renderer = std::env::var_os("EVORCH_RENDERER_EXE").ok_or("EVORCH_RENDERER_EXE")?;
    let profiles = std::path::PathBuf::from(
        std::env::var_os("EVORCH_EXPIRY_PROFILES").ok_or("EVORCH_EXPIRY_PROFILES")?,
    );
    let tasks = AreaTasks::new(tokio::runtime::Handle::current());
    let ledger = sandbox::SpawnLedger::new();
    let owner = RendererOwner::new(&tasks, ledger, OwnerConfig::new(renderer.into(), profiles))
        .map_err(|e| e.to_string())?;
    let pages = RuntimePages::new(Some(owner.clone()));
    let record = ArtifactRecord {
        artifact_id: "artifact-1".into(),
        sha256: "0".repeat(64),
        media_type: "text/html".into(),
        extension: "html".into(),
        title: "page".into(),
        caption: None,
        source_path: "/unused".into(),
        byte_len: 0,
        run_id: "run-1".into(),
        root_run_id: "run-1".into(),
        thread_id: Some("thread-1".into()),
        inlined_assets: Vec::new(),
    };
    let html = format!(
        "<!doctype html><html><body><script>var keep=[];for(var i=0;i<{n};i++){{var a=new Uint8Array(64*1024*1024);a.fill(i+1);keep.push(a);}}document.body.innerHTML='<h1>{CEF_HEADING}</h1>';</script></body></html>",
        n = CEF_SCRIPT_MIB / 64,
    );
    let page = pages
        .open_artifact("run-1", html.into_bytes(), &record)
        .await
        .map_err(|e| e.to_string())?;
    let text = page
        .agent()
        .snapshot()
        .await
        .map_err(|e| e.to_string())?
        .text;
    if !text.contains(CEF_HEADING) {
        return Err(format!("page was not loaded: {text:?}"));
    }
    say(&format!("NAVIGATED cef {CEF_HEADING}"));
    Ok(Box::new((tasks, owner, pages, page)))
}
