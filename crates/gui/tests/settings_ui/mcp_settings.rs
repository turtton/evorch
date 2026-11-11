//! Project settings MCP section: CRUD through the GUI service, off the render thread.
//!
//! The fake service performs real `config::mcp` loads and saves (the same
//! contract the production backend exposes) and records every privileged
//! call, so these tests check what the dialog asks the service to do and what
//! lands on disk, not how the backend is built.

use std::collections::BTreeSet;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

use async_trait::async_trait;
use config::mcp::{McpConfigError, McpOverrideMutation, McpSource, SourceRevision};
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::mcp::{
    ConfirmMcpApproval, McpApprovalPreview, McpGuiService, McpServerStatus, McpSettingsView,
    McpStatusEvent, SaveMcpSecret,
};
use gui::mcp_settings::{
    APPLY_NOTICE, CLOSE_MCP_LABEL, DraftField, DraftPair, DraftTransport, DraftValue, DraftWarning,
    MCP_SERVERS_BUTTON_LABEL, McpNotice, McpServerDraft, SAVE_SERVER_LABEL,
};
use gui::{app::WorkbenchState, model::project_dialog::ProjectDialog};
use tokio::sync::{Semaphore, watch};
use tools::mcp::McpErrorKind;
use tools::mcp::transport::CancelContext;
use tools::mcp::types::ApprovalScope;
use workspace_ui::{ProjectId, SidebarState};

/// Config-backed service; any approval, connection or secret call is a failure.
struct FakeService {
    gate: Semaphore,
    entered: Mutex<Option<mpsc::Sender<ThreadId>>>,
    settings_threads: Mutex<Vec<ThreadId>>,
    settings_calls: AtomicUsize,
    save_calls: AtomicUsize,
    privileged_calls: AtomicUsize,
    statuses: watch::Sender<Vec<McpStatusEvent>>,
}

impl FakeService {
    fn open() -> Arc<Self> {
        Self::with_permits(Semaphore::MAX_PERMITS)
    }

    fn with_permits(permits: usize) -> Arc<Self> {
        Arc::new(Self {
            gate: Semaphore::new(permits),
            entered: Mutex::new(None),
            settings_threads: Mutex::default(),
            settings_calls: AtomicUsize::new(0),
            save_calls: AtomicUsize::new(0),
            privileged_calls: AtomicUsize::new(0),
            statuses: watch::channel(Vec::new()).0,
        })
    }

    fn privileged(&self) -> McpErrorKind {
        self.privileged_calls.fetch_add(1, Ordering::SeqCst);
        McpErrorKind::Unavailable
    }

    fn view(&self, root: &Path) -> Result<McpSettingsView, McpErrorKind> {
        let config = config::mcp::load_project(root).map_err(kind)?;
        Ok(McpSettingsView {
            config,
            statuses: self.statuses.borrow().clone(),
        })
    }
}

fn kind(error: McpConfigError) -> McpErrorKind {
    runtime::mcp::mcp_config_error_kind(error)
}

#[async_trait]
impl McpGuiService for FakeService {
    async fn settings(&self, canonical_root: PathBuf) -> Result<McpSettingsView, McpErrorKind> {
        self.settings_calls.fetch_add(1, Ordering::SeqCst);
        let thread = std::thread::current().id();
        self.settings_threads.lock().unwrap().push(thread);
        if let Some(entered) = self.entered.lock().unwrap().as_ref() {
            let _ = entered.send(thread);
        }
        self.gate.acquire().await.expect("gate").forget();
        self.view(&canonical_root)
    }

    async fn save_override(
        &self,
        canonical_root: PathBuf,
        expected_revision: SourceRevision,
        mutation: McpOverrideMutation,
    ) -> Result<McpSettingsView, McpErrorKind> {
        self.save_calls.fetch_add(1, Ordering::SeqCst);
        config::mcp::save_override(&canonical_root, expected_revision, mutation).map_err(kind)?;
        self.view(&canonical_root)
    }

    fn subscribe_status(&self, _canonical_root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>> {
        self.statuses.subscribe()
    }

    async fn preview_approval(
        &self,
        _scope: ApprovalScope,
        _expected_revision: SourceRevision,
    ) -> Result<McpApprovalPreview, McpErrorKind> {
        Err(self.privileged())
    }

    async fn confirm_approval(
        &self,
        _confirmation: ConfirmMcpApproval,
    ) -> Result<(), McpErrorKind> {
        Err(self.privileged())
    }

    async fn revoke_approval(&self, _scope: ApprovalScope) -> Result<(), McpErrorKind> {
        Err(self.privileged())
    }

    async fn test_connection(
        &self,
        _scope: ApprovalScope,
        _expected_revision: SourceRevision,
        _cancel: CancelContext,
    ) -> Result<McpServerStatus, McpErrorKind> {
        Err(self.privileged())
    }

    async fn save_secret(&self, _request: SaveMcpSecret) -> Result<McpSettingsView, McpErrorKind> {
        Err(self.privileged())
    }
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    harness: HeadlessWorkbench<DemoSource>,
    service: Arc<FakeService>,
}

impl Fixture {
    fn new(json: Option<&str>, toml: Option<&str>, service: Arc<FakeService>) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join(".evorch")).expect("project");
        if let Some(json) = json {
            std::fs::write(root.join(".mcp.json"), json).expect(".mcp.json");
        }
        if let Some(toml) = toml {
            std::fs::write(root.join(".evorch/mcp.toml"), toml).expect("mcp.toml");
        }
        let mut sidebar = SidebarState::default();
        let id = ProjectId::new("demo");
        sidebar.add_project(id.clone(), "demo", &root).expect("add");
        sidebar.select_project(&id).expect("select");
        let state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .expect("state")
                .with_sidebar(sidebar)
                .with_mcp_settings_service(service.clone(), None);
        Self {
            _temp: temp,
            root,
            harness: HeadlessWorkbench::new(state, [1200.0, 1600.0]),
            service,
        }
    }

    fn open_settings(&mut self) {
        self.harness
            .state_mut()
            .open_project_settings(ProjectId::new("demo"));
        self.harness.run();
        self.click(MCP_SERVERS_BUTTON_LABEL);
    }

    /// Waits for the background load/save the last frame started, then redraws.
    fn settle(&mut self) {
        self.harness.state_mut().mcp_settings_mut().wait_idle();
        self.harness.run();
    }

    fn click(&mut self, label: &str) {
        self.harness.scroll_label_into_view(label);
        self.harness.run();
        self.harness.click_label(label);
        self.harness.run();
        self.settle();
    }

    fn json_bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.root.join(".mcp.json")).ok()
    }

    fn toml(&self) -> toml::Table {
        let text = std::fs::read_to_string(self.root.join(".evorch/mcp.toml")).unwrap_or_default();
        text.parse().expect("mcp.toml")
    }

    fn server_table(&self, id: &str) -> Option<toml::Table> {
        self.toml().get("servers")?.get(id)?.as_table().cloned()
    }
}

const IMPORTED: &str = r#"{"mcpServers": {"files": {"command": "node", "args": ["./server.js"], "env": {"TOKEN": "${FILES_TOKEN}"}}}}"#;

#[test]
fn open_and_save_never_connect() {
    // Given: a stdio server that would leave a marker if launched and an HTTP
    // server pointing at a listener that records connections.
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    listener.set_nonblocking(true).expect("nonblocking");
    let port = listener.local_addr().expect("addr").port();
    let temp_marker = tempfile::tempdir().expect("marker dir");
    let marker = temp_marker.path().join("spawned");
    let toml = format!(
        r#"version = 1
[servers.spawn]
type = "stdio"
command = "/bin/sh"
args = ["-c", "touch {}"]
[servers.remote]
type = "http"
url = "http://127.0.0.1:{port}/mcp"
"#,
        marker.display()
    );
    let mut fixture = Fixture::new(None, Some(&toml), FakeService::open());
    // When: opening settings, then saving one edit and one enable toggle.
    fixture.open_settings();
    assert!(fixture.harness.has_label(APPLY_NOTICE));
    fixture.click("Edit remote");
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .editor_mut()
        .expect("editor")
        .headers
        .push(DraftPair {
            name: "X-Tenant".into(),
            value: DraftValue::text("${MCP_TENANT:-default}"),
        });
    fixture.harness.run();
    fixture.click(SAVE_SERVER_LABEL);
    fixture.click("Disable spawn");
    // Then: both saves happened, yet nothing was launched or connected and no
    // approval, connection test or secret call was made.
    let state = fixture.harness.state();
    assert_eq!(state.mcp_settings().notice(), Some(&McpNotice::Saved));
    assert_eq!(fixture.service.save_calls.load(Ordering::SeqCst), 2);
    assert_eq!(fixture.service.privileged_calls.load(Ordering::SeqCst), 0);
    assert!(!marker.exists(), "stdio command must not run");
    assert_eq!(
        listener.accept().map(|_| ()).map_err(|error| error.kind()),
        Err(std::io::ErrorKind::WouldBlock),
        "HTTP server must not be contacted"
    );
    assert!(
        fixture.json_bytes().is_none(),
        ".mcp.json must not be created"
    );
    let remote = fixture.server_table("remote").expect("remote");
    assert_eq!(
        remote["headers"]["X-Tenant"].as_str(),
        Some("${MCP_TENANT:-default}")
    );
    let spawn = fixture.server_table("spawn").expect("spawn");
    assert_eq!(spawn["enabled"].as_bool(), Some(false));
    assert_eq!(spawn["command"].as_str(), Some("/bin/sh"));
}

#[test]
fn imported_edits_write_complete_toml_override() {
    // Given: a .mcp.json server and no mcp.toml.
    let mut fixture = Fixture::new(Some(IMPORTED), None, FakeService::open());
    fixture.open_settings();
    let before = fixture.json_bytes();
    assert!(fixture.harness.has_label(".mcp.json"));
    // When: editing one field of the imported server and saving.
    fixture.click("Edit files");
    let draft = fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .editor_mut()
        .expect("editor");
    assert!(draft.imported);
    draft.args.push("--verbose".into());
    draft.roles.insert("reviewer".into());
    fixture.harness.run();
    fixture.click(SAVE_SERVER_LABEL);
    // Then: .mcp.json is byte-identical and mcp.toml holds the whole server,
    // including the unchanged command and env reference.
    assert_eq!(fixture.json_bytes(), before);
    let files = fixture.server_table("files").expect("override");
    assert_eq!(files["type"].as_str(), Some("stdio"));
    assert_eq!(files["command"].as_str(), Some("node"));
    let args: Vec<_> = files["args"]
        .as_array()
        .expect("args")
        .iter()
        .map(|arg| arg.as_str().expect("arg"))
        .collect();
    assert_eq!(args, ["./server.js", "--verbose"]);
    assert_eq!(files["env"]["TOKEN"].as_str(), Some("${FILES_TOKEN}"));
    let roles: Vec<_> = files["roles"]
        .as_array()
        .expect("roles")
        .iter()
        .map(|role| role.as_str().expect("role"))
        .collect();
    assert_eq!(roles, ["worker", "reviewer"]);
    let loaded = config::mcp::load_project(&fixture.root).expect("load");
    let entry = &loaded.servers["files"];
    assert_eq!(entry.source, McpSource::ProjectToml);
    assert!(entry.imported_exists);
    assert!(fixture.harness.has_label("override"));
    assert!(fixture.harness.state().mcp_settings().editor().is_none());
}

#[test]
fn disable_and_remove_override_are_distinct() {
    // Given: an imported server and a server defined only in mcp.toml.
    let toml =
        "version = 1\n[servers.local]\ntype = \"http\"\nurl = \"https://mcp.example.com/mcp\"\n";
    let mut fixture = Fixture::new(Some(IMPORTED), Some(toml), FakeService::open());
    fixture.open_settings();
    let before = fixture.json_bytes();
    // When: disabling the imported server.
    fixture.click("Disable files");
    // Then: mcp.toml gets a tombstone, not a copy of the definition.
    let tombstone = fixture.server_table("files").expect("tombstone");
    assert_eq!(tombstone.len(), 1);
    assert_eq!(tombstone["enabled"].as_bool(), Some(false));
    let loaded = config::mcp::load_project(&fixture.root).expect("load");
    assert!(loaded.servers["files"].tombstone);
    // When: removing the override.
    fixture.click("Remove override files");
    // Then: the table is gone and the .mcp.json definition applies again.
    assert!(fixture.server_table("files").is_none());
    let loaded = config::mcp::load_project(&fixture.root).expect("load");
    assert_eq!(loaded.servers["files"].source, McpSource::ImportedJson);
    assert!(loaded.servers["files"].definition.is_some());
    // When: disabling the mcp.toml-only server, then deleting it.
    fixture.click("Disable local");
    // Then: disabling keeps its definition; deleting asks first, then removes it.
    let local = fixture.server_table("local").expect("local");
    assert_eq!(local["enabled"].as_bool(), Some(false));
    assert_eq!(local["url"].as_str(), Some("https://mcp.example.com/mcp"));
    fixture.click("Delete local");
    assert!(fixture.server_table("local").is_some());
    fixture.click("Confirm delete local");
    assert!(fixture.server_table("local").is_none());
    assert_eq!(fixture.json_bytes(), before);
    assert_eq!(fixture.service.privileged_calls.load(Ordering::SeqCst), 0);
}

#[test]
fn save_conflict_is_reported_without_overwriting() {
    // Given: settings loaded, then mcp.toml edited outside the dialog.
    let mut fixture = Fixture::new(Some(IMPORTED), None, FakeService::open());
    fixture.open_settings();
    let external =
        "version = 1\n[servers.other]\ntype = \"http\"\nurl = \"https://example.com/mcp\"\n";
    std::fs::write(fixture.root.join(".evorch/mcp.toml"), external).expect("external edit");
    // When: saving a change based on the old revision.
    fixture.click("Disable files");
    // Then: the user sees the conflict and the external edit is untouched.
    assert_eq!(
        fixture.harness.state().mcp_settings().notice(),
        Some(&McpNotice::Conflict)
    );
    assert_eq!(
        std::fs::read_to_string(fixture.root.join(".evorch/mcp.toml")).expect("toml"),
        external
    );
    // When: reloading and saving again.
    fixture.click("Reload");
    fixture.click("Disable files");
    // Then: the save applies on top of the external edit.
    assert!(fixture.server_table("other").is_some());
    assert_eq!(
        fixture.server_table("files").expect("tombstone")["enabled"].as_bool(),
        Some(false)
    );
}

#[test]
fn render_thread_does_not_perform_io() {
    // Given: a service whose load blocks until released.
    let service = FakeService::with_permits(0);
    let (entered_tx, entered_rx) = mpsc::channel();
    *service.entered.lock().unwrap() = Some(entered_tx);
    let mut fixture = Fixture::new(Some(IMPORTED), None, service.clone());
    // When: opening settings and drawing frames while the load is blocked.
    fixture
        .harness
        .state_mut()
        .open_project_settings(ProjectId::new("demo"));
    fixture.harness.run();
    assert_eq!(service.settings_calls.load(Ordering::SeqCst), 0);
    fixture.harness.click_label(MCP_SERVERS_BUTTON_LABEL);
    fixture.harness.run();
    let load_thread = entered_rx.recv().expect("load started");
    // Then: frames keep rendering a loading state; the load runs elsewhere.
    assert_ne!(load_thread, std::thread::current().id());
    assert!(fixture.harness.has_label("Loading MCP servers…"));
    assert!(fixture.harness.state().mcp_settings().is_busy());
    fixture.harness.run();
    assert_eq!(service.settings_calls.load(Ordering::SeqCst), 1);
    // When: the load finishes.
    service.gate.add_permits(1);
    fixture.settle();
    // Then: the list shows the server.
    assert!(fixture.harness.has_label("Edit files"));
    assert!(matches!(
        fixture.harness.state().project_dialog(),
        ProjectDialog::Settings { .. }
    ));
    // When: closing the MCP dialog.
    fixture.harness.click_label(CLOSE_MCP_LABEL);
    fixture.harness.run();
    // Then: the MCP session is dropped without further service calls and the
    // project settings stay open.
    assert!(fixture.harness.state().mcp_settings().root().is_none());
    assert_eq!(service.settings_calls.load(Ordering::SeqCst), 1);
    assert!(matches!(
        fixture.harness.state().project_dialog(),
        ProjectDialog::Settings { .. }
    ));
}

#[test]
fn editor_validates_input_and_defaults_to_worker() {
    let taken = BTreeSet::from(["files".to_owned()]);
    // A new server defaults to Worker and must name a fresh ID and a command.
    let mut draft = McpServerDraft::new_server();
    assert_eq!(draft.roles, BTreeSet::from(["worker".to_owned()]));
    draft.id = "files".into();
    let check = draft.check(&taken);
    assert!(check.error(DraftField::Id).is_some());
    assert!(check.error(DraftField::Command).is_some());
    assert!(!check.is_valid());
    // Bad references and timeouts are reported on their own fields.
    draft.id = "search".into();
    draft.command = "${UNCLOSED".into();
    draft.startup_timeout_ms = "0".into();
    let check = draft.check(&taken);
    assert!(check.error(DraftField::Command).is_some());
    assert!(check.error(DraftField::StartupTimeout).is_some());
    // HTTP: token and Authorization exclude each other; managed headers are refused.
    draft.command = "node".into();
    draft.startup_timeout_ms = "10000".into();
    draft.transport = DraftTransport::Http;
    draft.url = "https://mcp.example.com/mcp".into();
    draft.token = Some(DraftValue::text("${API_TOKEN}"));
    draft.headers = vec![
        DraftPair {
            name: "Authorization".into(),
            value: DraftValue::text("Bearer x"),
        },
        DraftPair {
            name: "Mcp-Method".into(),
            value: DraftValue::text("x"),
        },
    ];
    let check = draft.check(&taken);
    assert!(check.error(DraftField::HeaderName(0)).is_some());
    assert!(check.error(DraftField::HeaderName(1)).is_some());
    // A literal token is valid but warns that it is stored in plain text.
    draft.headers.clear();
    draft.token = Some(DraftValue::text("sk-live-123"));
    let check = draft.check(&taken);
    assert!(check.is_valid(), "{:?}", check.errors);
    assert!(
        check
            .warnings
            .iter()
            .any(|warning| matches!(warning, DraftWarning::PlainTextSecret(_)))
    );
    // Keychain references save only the name and do not warn.
    draft.token = Some(DraftValue::keychain("remote-token"));
    let check = draft.check(&taken);
    assert!(check.is_valid());
    assert!(check.warnings.is_empty(), "{:?}", check.warnings);
    // userinfo in the URL is rejected.
    draft.url = "https://user@mcp.example.com/mcp".into();
    assert!(draft.check(&taken).error(DraftField::Url).is_some());
}
