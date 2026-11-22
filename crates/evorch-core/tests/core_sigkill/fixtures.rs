//! External dependencies supplied to unchanged product callers.
use super::product::{quote, script};
use mock_openai::{ScriptedResponse, StreamingMockOpenAi};
use serde_json::json;
use std::{
    fs, io,
    path::{Path, PathBuf},
};
pub struct Fixture {
    pub bindir: PathBuf,
    pub cli: Option<PathBuf>,
    pub mock: StreamingMockOpenAi,
}
pub async fn prepare(
    cell: &str,
    root: &Path,
    project: &Path,
    own: &Path,
    socket: &Path,
) -> io::Result<Fixture> {
    // PTY master close must not mask an absent guard with natural SIGHUP death.
    // POSIX sh (including dash) preserves ignored HUP across exec and fork.
    let launch_peer = format!(
        "trap '' HUP; exec {} --peer root {}",
        quote(own),
        quote(socket)
    );
    let bindir = root.join("bin");
    fs::create_dir(&bindir)?;
    if cell == "eval" {
        super::evaluation::repository(project).await?;
        script(
            &bindir.join("evorch"),
            &format!(
                "printf '%s\\n' \"$@\" > {}\n{launch_peer}",
                quote(&project.join(".eval-argv"))
            ),
        )?;
    }
    let selector = match cell {
        "grep" => Some(("rg", String::new())),
        "git_diff" => Some((
            "git",
            "case \" $* \" in *' diff '*) ;; *) exec REAL \"$@\";; esac".into(),
        )),
        "git_read" => Some(("git", String::new())),
        "bwrap_probe" => Some(("bwrap", String::new())),
        "libraries" => Some((
            "bwrap",
            r#"case " $* " in *' --list '*) ;; *) exec REAL "$@";; esac"#.into(),
        )),
        "pull_request" => Some(("gh", String::new())),
        "comment_checker" => Some(("comment-checker", String::new())),
        _ => None,
    };
    if let Some((name, mut prefix)) = selector {
        if prefix.contains("REAL") {
            let real = (name == "bwrap")
                .then(|| std::env::var_os("EVORCH_B3_BWRAP_EXE"))
                .flatten()
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::split_paths(&std::env::var_os("PATH").unwrap())
                        .map(|p| p.join(name))
                        .find(|p| p.is_file())
                })
                .ok_or_else(|| {
                    io::Error::other(format!(
                        "{name} missing; bwrap can be supplied via EVORCH_B3_BWRAP_EXE"
                    ))
                })?;
            prefix = prefix.replace("REAL", &quote(&real));
        }
        script(&bindir.join(name), &format!("{prefix}\n{launch_peer}"))?;
    }
    if cell == "mcp" {
        fs::create_dir_all(project.join(".evorch"))?;
        fs::write(
            project.join(".evorch/mcp.toml"),
            format!(
                "version=1\n[servers.fixture]\ntype=\"stdio\"\ncommand={own:?}\nargs=[\"--peer\",\"root\",{socket:?}]\nroles=[\"worker\"]\nstartup_timeout_ms=120000\n"
            ),
        )?;
    }
    let cli = if matches!(cell, "js_worker" | "libraries") {
        let cli = std::env::var_os("EVORCH_B3_CLI_EXE")
            .ok_or_else(|| io::Error::other("EVORCH_B3_CLI_EXE (actual evorch CLI) required"))?;
        let path = root.join("bin/evorch");
        fs::copy(cli, &path)?;
        Some(path)
    } else {
        None
    };
    let (tool, arguments) = match cell {
        "shell" | "shell_pty" => (
            "shell",
            json!({"command":launch_peer, "cwd":project, "interactive":cell == "shell_pty", "timeout_ms":120000}),
        ),
        "js_worker" | "libraries" => (
            "run_js",
            json!({"code":format!("try {{ await tools.shell({}); }} finally {{ await new Promise(() => {{}}); }}",json!({"command":launch_peer,"cwd":project})), "timeout_ms":120000}),
        ),
        "grep" => ("grep", json!({"pattern":"matrix", "path":project})),
        "git_diff" => ("git_diff", json!({"cwd":project})),
        "comment_checker" => (
            "write",
            json!({"path":project.join("note.rs"),"content":"// matrix\nfn main() {}\n"}),
        ),
        _ => ("shell", json!({"command":":", "cwd":project})),
    };
    let response = if matches!(cell, "mcp" | "pull_request") {
        ScriptedResponse::text_stream("matrix", "mock-model", ["ready"])
    } else {
        ScriptedResponse::tool_call(
            "matrix",
            "mock-model",
            0,
            "call",
            tool,
            [arguments.to_string()],
        )
    };
    let responses = if cell == "eval" {
        super::evaluation::responses()
    } else {
        vec![response]
    };
    let mock =
        StreamingMockOpenAi::spawn_with_prompt_cache_models(responses, vec!["mock-model".into()]);
    configure(root, project, &mock.base_url())?;
    Ok(Fixture { bindir, cli, mock })
}
fn configure(root: &Path, project: &Path, url: &str) -> io::Result<()> {
    fs::write(
        root.join("config.toml"),
        format!(
            r#"
[core]
idle_grace_secs = 3600
[providers.local]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{url}"
api_key_env = "B3_TEST_KEY"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.worker]]
profile = "local"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.evaluator]]
profile = "local"
[providers.candidate]
type = "openai-compatible"
api_protocol = "openai-completions"
base_url = "{url}"
api_key_env = "B3_TEST_KEY"
models = ["mock-model-candidate"]
default_model = "mock-model-candidate"
[tools.run_js]
enabled = true
scopes = ["worker"]
[pull_requests]
enabled = true
[comment_checker]
timeout_ms = 120000
"#
        ),
    )?;
    let cache = root.join("cache/evorch");
    fs::create_dir_all(&cache)?;
    fs::copy(
        test_support::model_catalog_dir().join("models-dev.json"),
        cache.join("models-dev.json"),
    )?;
    let mut sidebar = workspace_state::SidebarState::default();
    sidebar
        .add_project(workspace_state::ProjectId::new("p"), "matrix", project)
        .map_err(io::Error::other)?;
    sidebar
        .add_allowed_directory(
            &workspace_state::ProjectId::new("p"),
            &root.join("bin"),
            workspace_state::TrustState::Approved,
        )
        .map_err(io::Error::other)?;
    fs::write(root.join("sidebar.json"), serde_json::to_vec(&sidebar)?)
}
