//! Configuration of the unchanged product and its external dependencies:
//! a mock model provider, and for Chromium the cell's own NSS trust store.
use super::observer::bounded;
use std::{
    ffi::OsString,
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

/// An external executable that records its launch and exits at once. The core
/// is told to use it through its own `EVORCH_RENDERER_EXEC` setting.
pub fn failing_renderer(root: &Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let path = root.join("renderer-fail.sh");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\necho launched > '{}'\nexit 3\n",
            root.join("renderer-launched").display()
        ),
    )?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(path)
}

/// `LD_LIBRARY_PATH` that starts with a directory holding only a `libcef.so`
/// link: the renderer still links (every other library resolves through the
/// original path), but CEF finds none of its resources beside the library, so
/// its initialization fails. The library comes from the renderer's own link
/// resolution (`ldd`), so the cell does not depend on how the host sets CEF up.
pub fn cef_link_only(root: &Path, renderer: &Path) -> io::Result<OsString> {
    let listing = std::process::Command::new("ldd").arg(renderer).output()?;
    let text = String::from_utf8_lossy(&listing.stdout);
    let library = text
        .lines()
        .find_map(|line| {
            let line = line.trim();
            line.strip_prefix("libcef.so => ")
                .and_then(|rest| rest.split_whitespace().next())
                .map(PathBuf::from)
        })
        .filter(|path| path.is_file())
        .ok_or_else(|| io::Error::other("ldd does not resolve libcef.so for the renderer"))?;
    let dir = root.join("cef-link-only");
    fs::create_dir_all(&dir)?;
    std::os::unix::fs::symlink(library.canonicalize()?, dir.join("libcef.so"))?;
    let mut paths = vec![dir];
    paths.extend(std::env::split_paths(
        &std::env::var_os("LD_LIBRARY_PATH").unwrap_or_default(),
    ));
    std::env::join_paths(paths).map_err(io::Error::other)
}
