//! A product runs in directories of its own: config, state, cache, home,
//! temporary files and the socket directory are all created here and removed
//! when the value is dropped. The environment of the product is built from
//! nothing; only what the product needs to find its libraries and tools is
//! passed through.
use std::{
    ffi::OsString,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::Command,
};

/// Variables passed from the test process: the tool search path and the
/// dynamic library path of the development shell. Nothing else (no provider
/// key, no `EVORCH_*`, no `XDG_*`) leaks into the product.
const PASSTHROUGH: [&str; 2] = ["PATH", "LD_LIBRARY_PATH"];

pub fn passthrough(command: &mut Command) -> &mut Command {
    command.env_clear();
    for name in PASSTHROUGH {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
}

pub struct IsolatedEnv {
    root: tempfile::TempDir,
    sockets: test_socket_dir::SocketDir,
}

/// Where a removed environment used to live; kept to prove the removal.
#[derive(Debug, Clone)]
pub struct Footprint {
    pub root: PathBuf,
    pub sockets: PathBuf,
}
impl Footprint {
    pub fn remains(&self) -> Vec<PathBuf> {
        [&self.root, &self.sockets]
            .into_iter()
            .filter(|p| p.exists())
            .cloned()
            .collect()
    }
}

impl IsolatedEnv {
    #[track_caller]
    pub fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("evorch-f4p-")
            .tempdir()
            .expect("create isolated product root");
        for dir in ["config", "cache", "home", "tmp", "project"] {
            std::fs::create_dir(root.path().join(dir)).expect("create isolated directory");
        }
        let sockets = test_socket_dir::SocketDir::new();
        for dir in ["run", "state"] {
            let path = sockets.path().join(dir);
            std::fs::create_dir(&path).expect("create socket-side directory");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))
                .expect("restrict socket-side directory");
        }
        Self { root, sockets }
    }
    pub fn root(&self) -> &Path {
        self.root.path()
    }
    pub fn config_dir(&self) -> PathBuf {
        self.root().join("config")
    }
    /// `XDG_STATE_HOME`. It lives beside the runtime directory, under the short
    /// socket directory: the ownership sockets are created below it and a Unix
    /// socket path must stay under 108 bytes.
    pub fn state_dir(&self) -> PathBuf {
        self.sockets.path().join("state")
    }
    pub fn cache_dir(&self) -> PathBuf {
        self.root().join("cache")
    }
    pub fn home_dir(&self) -> PathBuf {
        self.root().join("home")
    }
    pub fn tmp_dir(&self) -> PathBuf {
        self.root().join("tmp")
    }
    pub fn project_dir(&self) -> PathBuf {
        self.root().join("project")
    }
    /// `XDG_RUNTIME_DIR`: short, 0700, holds the core's sockets.
    pub fn runtime_dir(&self) -> PathBuf {
        self.sockets.path().join("run")
    }
    /// The ownership registry the product opens (`$XDG_STATE_HOME/evorch/threads`).
    pub fn ownership_db(&self) -> PathBuf {
        self.state_dir().join("evorch/threads/owners.db")
    }
    /// Strings that are unique to this environment: a process whose command line
    /// or working directory contains one belongs to the product started here,
    /// even when it cleared its environment.
    pub fn ownership_markers(&self) -> Vec<String> {
        vec![
            self.root().to_string_lossy().into_owned(),
            self.sockets.path().to_string_lossy().into_owned(),
        ]
    }
    pub fn footprint(&self) -> Footprint {
        Footprint {
            root: self.root().to_owned(),
            sockets: self.sockets.path().to_owned(),
        }
    }
    /// The complete environment of a product started here.
    pub fn apply(&self, command: &mut Command, extra: &[(&str, OsString)]) {
        passthrough(command);
        command
            .env("HOME", self.home_dir())
            .env("XDG_RUNTIME_DIR", self.runtime_dir())
            .env("XDG_STATE_HOME", self.state_dir())
            .env("XDG_CACHE_HOME", self.cache_dir())
            .env("XDG_CONFIG_HOME", self.root().join("xdg-config"))
            .env("XDG_DATA_HOME", self.root().join("xdg-data"))
            .env("TMPDIR", self.tmp_dir())
            .current_dir(self.project_dir());
        for (name, value) in extra {
            command.env(name, value);
        }
    }

    /// Provider, routing, project and model catalog for a product whose only
    /// provider is the given OpenAI-compatible base URL. The key is read from
    /// `key_env`; the caller passes that variable to the product.
    pub fn write_product_config(&self, base_url: &str, key_env: &str) {
        let config = self.config_dir();
        std::fs::write(
            config.join("config.toml"),
            format!(
                r#"
[providers.local]
type = "openai-compatible"
base_url = "{base_url}"
api_key_env = "{key_env}"
models = ["mock-model"]
default_model = "mock-model"
[[routing.routes.orchestrator]]
profile = "local"
[[routing.routes.worker]]
profile = "local"
[compaction]
context_window_tokens = 1000000
"#
            ),
        )
        .expect("write product config");
        let catalog = test_support::model_catalog_dir().join("models-dev.json");
        let cache = self.cache_dir().join("evorch");
        std::fs::create_dir_all(&cache).expect("create cache dir");
        std::fs::copy(&catalog, cache.join("models-dev.json")).expect("seed model catalog");
        // Registered projects resolve their catalog beside the config.
        let recovery = config.join("catalog");
        std::fs::create_dir(&recovery).expect("create catalog dir");
        std::fs::copy(&catalog, recovery.join("models-dev.json")).expect("seed model catalog");
        let mut sidebar = workspace_state::SidebarState::default();
        sidebar
            .add_project(
                workspace_state::ProjectId::new("p"),
                "f4p fixture",
                &self.project_dir(),
            )
            .expect("register project");
        std::fs::write(
            config.join("sidebar.json"),
            serde_json::to_vec(&sidebar).expect("serialize sidebar"),
        )
        .expect("write sidebar");
    }
}
