//! T evidence: explicitly injected/API callers; never product-ELF coverage.
use sandbox::{CommandSpec, DirectSandbox, SpawnLedger};
use serde_json::json;
use std::{io, path::Path, sync::Arc, time::Duration};
use tools::tools::run_js::{ChildCall, ChildOutcome, RunJs, RunJsHost, WorkerLauncher, normalize};
use tools::{Tool, ToolExecutionContext, ToolExecutor};

struct Host(Arc<ToolExecutor>);
impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MatrixHost")
    }
}
#[async_trait::async_trait]
impl RunJsHost for Host {
    fn tool_names(&self) -> Vec<String> {
        vec!["shell".into()]
    }
    fn settings(&self) -> config::RunJsConfig {
        config::RunJsConfig {
            enabled: true,
            ..Default::default()
        }
    }
    fn shares_wave(&self, _: &str) -> bool {
        false
    }
    async fn call(&self, c: ChildCall) -> ChildOutcome {
        let context = ToolExecutionContext {
            run_id: "matrix".into(),
            parent_call_id: Some(c.parent_call_id.clone()),
            ..Default::default()
        };
        let result = self
            .0
            .execute(
                &context,
                &c.name,
                &format!("{}/{}", c.parent_call_id, c.seq),
                c.args.clone(),
            )
            .await;
        let (result, is_error) = match result {
            Ok(r) => (
                Ok(normalize::normalize(&c.name, &c.args, &r).into_value()),
                r.is_error,
            ),
            Err(e) => (Err(e.agent_message()), true),
        };
        ChildOutcome {
            result,
            is_error,
            paths: vec![],
            written: vec![],
            failure: None,
            warning: None,
        }
    }
}

pub async fn owner(cell: &str, root: &Path, peer: &Path, socket: &Path) -> io::Result<()> {
    let ledger = SpawnLedger::new();
    let report = ledger.clone();
    tokio::spawn(async move {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
        if lines.next_line().await.ok().flatten().as_deref() == Some("LEDGER") {
            let text = format!(
                "{}\n",
                json!({"registered":report.children().len(),"children":report.children().iter().map(|child|json!({"kind":child.kind.as_str(),"leader":child.leader})).collect::<Vec<_>>()})
            );
            let _ = tokio::io::stdout().write_all(text.as_bytes()).await;
        }
    });
    let direct = Arc::new(DirectSandbox::new_unchecked());
    let mut spec = CommandSpec {
        program: peer.to_string_lossy().into(),
        args: vec![],
        cwd: Some(root.into()),
        extra_env: vec![],
    };
    spec.args = vec![
        "--peer".into(),
        "root".into(),
        socket.to_string_lossy().into(),
    ];
    fn error(e: impl std::fmt::Debug) -> io::Error {
        io::Error::other(format!("{e:?}"))
    }
    match cell {
        "lsp_t" => {
            let file = root.join("test.rs");
            std::fs::write(&file, "fn main(){}")?;
            let tool = tools::lsp::LspDiagnostics::new(direct, spec, "rust".into())
                .with_ledger(ledger)
                .with_timeout(Duration::from_secs(120));
            tool.execute(json!({"path":file})).await.map_err(error)?;
        }
        "quota_t" => {
            use providers::provider::codex::{
                quota::{CodexQuotaClient, QuotaConfig},
                tokens::InMemoryTokenStore,
            };
            let mut client = CodexQuotaClient::new(
                QuotaConfig {
                    app_server_program: peer.into(),
                    app_server_args: spec.args,
                    timeout: Duration::from_secs(120),
                    wham_endpoint: "http://127.0.0.1:1/unused".into(),
                    ledger: Some(ledger),
                    ..Default::default()
                },
                Arc::new(InMemoryTokenStore::new()),
            )
            .map_err(error)?;
            client.fetch_quota().await.map_err(error)?;
        }
        "legacy_stdio_t" | "probe_t" => {
            // Linux no longer has the raw StdioSession API. Exercise its owned
            // stdio adapter, not a fabricated product caller or legacy raw API.
            let kind = if cell == "probe_t" {
                sandbox::ChildKind::Probe
            } else {
                sandbox::ChildKind::Other
            };
            let mut command = sandbox::owned::OwnedCommand::new(peer);
            command
                .args(&spec.args)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped());
            let session = sandbox::owned::OwnedSession::spawn_owned(
                Some(&ledger),
                kind,
                Some(cell),
                &mut command,
            )?;
            session.output(None).map_err(error)?;
        }
        "chromium_t" => {
            struct Policy;
            #[async_trait::async_trait]
            impl browser::agent::RequestPolicy for Policy {
                async fn check(&self, _: &reqwest::Url) -> Result<(), String> {
                    Ok(())
                }
                async fn resolve(
                    &self,
                    _: &str,
                    _: u16,
                    _: bool,
                ) -> Result<Vec<std::net::SocketAddr>, String> {
                    Ok(vec![])
                }
            }
            use browser::agent::{AgentBrowser, LaunchOptions, PageProvider};
            let browser = AgentBrowser::new(LaunchOptions {
                executable: Some(root.join("external-peer")),
                policy: Arc::new(Policy),
                max_pages: 1,
                max_tabs: 1,
            });
            browser.page("matrix").await.map_err(error)?;
        }
        "renderer_t" => {
            use workbench_host::areas::{
                AreaTasks,
                browser::renderer::{OwnerConfig, RendererOwner},
            };
            let tasks = AreaTasks::new(tokio::runtime::Handle::current());
            let mut config = OwnerConfig::new(root.join("external-peer"), root.join("profiles"));
            config.hello_timeout = Duration::from_secs(120);
            let renderer = RendererOwner::new(&tasks, ledger, config).map_err(error)?;
            let request=serde_json::from_value(json!({"session":1,"tab":1,"viewport":{"width":4,"height":2},"frame_rate":1,"files":[{"path":"index.html","mime":"text/html","len":1}],"timeout_ms":120000})).map_err(error)?;
            renderer.open(request, vec![b'x']).await.map_err(error)?;
        }
        "shell_direct_t" | "js_direct_t" => {
            let mut executor = ToolExecutor::with_standard_tools_in(
                Arc::new(event_bus::EventBus::new(256)),
                direct.clone(),
                None,
            )
            .with_spawn_ledger(ledger);
            if cell == "js_direct_t" {
                let cli = std::env::var_os("EVORCH_B3_CLI_EXE")
                    .ok_or_else(|| io::Error::other("EVORCH_B3_CLI_EXE required"))?;
                executor
                    .register(Arc::new(RunJs::with_launcher(WorkerLauncher::injected(
                        direct,
                        cli.into(),
                        vec!["js-worker".into()],
                    ))))
                    .map_err(error)?;
            }
            let executor = Arc::new(executor);
            let command = format!(
                "exec {} --peer root {}",
                super::product::quote(peer),
                super::product::quote(socket)
            );
            let (tool, args) = if cell == "shell_direct_t" {
                (
                    "shell",
                    json!({"command":command,"cwd":root,"timeout_ms":120000}),
                )
            } else {
                (
                    "run_js",
                    json!({"code":format!("try {{ await tools.shell({}); }} finally {{ await new Promise(()=>{{}}); }}",json!({"command":command,"cwd":root})),"timeout_ms":120000}),
                )
            };
            let context = ToolExecutionContext {
                run_id: "matrix".into(),
                run_js_host: Some(Arc::new(Host(executor.clone()))),
                ..Default::default()
            };
            executor
                .execute(&context, tool, "call", args)
                .await
                .map_err(error)?;
        }
        _ => return Err(io::Error::other("unknown T cell")),
    }
    Err(io::Error::other(
        "T caller returned before controller SIGKILL",
    ))
}
