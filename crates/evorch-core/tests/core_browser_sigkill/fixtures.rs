//! Configuration of the unchanged product and its external dependencies:
//! a mock model provider, and for Chromium the cell's own NSS trust store.
use super::observer::bounded;
use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

pub fn write_config(root: &Path, project: &Path, url: &str, browser: &str) -> io::Result<()> {
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
{browser}
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
        .add_project(workspace_state::ProjectId::new("p"), "b3-08", project)
        .map_err(io::Error::other)?;
    fs::write(root.join("sidebar.json"), serde_json::to_vec(&sidebar)?)
}

fn find_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

pub fn require_absolute_file(variable: &str) -> io::Result<PathBuf> {
    let path = std::env::var_os(variable)
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::other(format!("{variable} is required (the cell never skips)"))
        })?;
    if !path.is_absolute() || !path.is_file() {
        return Err(io::Error::other(format!(
            "{variable}={} is not an absolute existing file",
            path.display()
        )));
    }
    Ok(path)
}

async fn certutil(certutil: &Path, home: &Path, args: &[&str]) -> io::Result<()> {
    let mut command = tokio::process::Command::new(certutil);
    command
        .args(args)
        .env_clear()
        .env("HOME", home)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    let output = bounded(Duration::from_secs(30), "certutil", command.output()).await?;
    if output.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "certutil {args:?}: {} {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )))
    }
}

/// The only trust Chromium gets: `<home>/.pki/nssdb`, under the cell's own
/// temporary HOME. A missing certutil fails the cell. `root` is the cell's
/// private directory; the store is refused anywhere else, so a developer's or
/// runner's real `~/.pki` is never touched.
pub async fn trust_ca(root: &Path, home: &Path, ca_pem: &str) -> io::Result<PathBuf> {
    if !home.starts_with(root) || home.parent() == Some(Path::new("/")) {
        return Err(io::Error::other(
            "nssdb must live under the cell's own HOME",
        ));
    }
    let exe = match std::env::var_os("EVORCH_B3_CERTUTIL") {
        Some(path) => PathBuf::from(path),
        None => find_in_path("certutil")
            .ok_or_else(|| io::Error::other("certutil (libnss3-tools) is required"))?,
    };
    let db = home.join(".pki/nssdb");
    fs::create_dir_all(&db)?;
    let ca = root.join("b3-08-ca.pem");
    fs::write(&ca, ca_pem)?;
    let sql = format!("sql:{}", db.display());
    certutil(&exe, home, &["-N", "-d", &sql, "--empty-password"]).await?;
    certutil(
        &exe,
        home,
        &[
            "-A",
            "-d",
            &sql,
            "-n",
            "b3-08-ca",
            "-t",
            "C,,",
            "-i",
            ca.to_str().unwrap(),
        ],
    )
    .await?;
    Ok(db)
}
