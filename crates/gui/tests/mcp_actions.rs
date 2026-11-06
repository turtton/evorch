//! MCP approval, keychain secrets, connection tests and run-start notices in
//! the project settings dialog.
//!
//! The fake service keeps a real `config::mcp` project on disk, an in-memory
//! approval ledger and keychain, and records every privileged call. The tests
//! check what the dialog asks the service to do, what lands on disk and what
//! the user is shown; they do not check how the production backend works.

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use config::mcp::{McpOverrideMutation, McpTransportDefinition, SourceRevision};
use gui::fixture::DemoSource;
use gui::headless::HeadlessWorkbench;
use gui::mcp::{
    ConfirmMcpApproval, McpApprovalPreview, McpGuiService, McpServerStatus, McpSettingsView,
    McpStatusEvent, SaveMcpSecret,
};
use gui::mcp_settings::{
    APPROVE_LABEL, DraftTransport, DraftValue, MCP_SERVERS_BUTTON_LABEL, McpApprovalState,
    McpNotice, McpRowNote, McpServerDraft, McpSettingsAction, NEXT_RUN_NOTICE, NOTICE_REVIEW_LABEL,
    OUTSIDE_SANDBOX_WARNING, REVIEW_AGAIN_LABEL, STALE_PREVIEW_TEXT, STORE_SECRET_LABEL,
    SecretTarget, TEST_REFUSED_TEXT, UNVERIFIED_CONTENT_NOTE, secret_error_text,
};
use gui::model::commands::{CommandSink, LoopEvent, WorkbenchCommand};
use gui::{app::WorkbenchState, model::project_dialog::ProjectDialog};
use tokio::sync::watch;
use tools::mcp::McpErrorKind;
use tools::mcp::transport::CancelContext;
use tools::mcp::types::{
    ApprovalFingerprint, ApprovalPreviewId, ApprovalScope, ApprovalWarning, McpApprovalSummary,
    McpDestinationSummary,
};
use workspace_ui::{ProjectId, SidebarState};

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Settings,
    Save,
    Preview,
    Confirm {
        preview_id: ApprovalPreviewId,
        revision: SourceRevision,
        fingerprint: ApprovalFingerprint,
    },
    Revoke,
    Test,
    SaveSecret {
        logical_name: String,
    },
}

#[derive(Default)]
struct Ledger {
    /// Last preview handed out per server: (id, revision, fingerprint).
    previews: BTreeMap<String, (ApprovalPreviewId, SourceRevision, ApprovalFingerprint)>,
    approved: BTreeMap<String, ApprovalFingerprint>,
    next_preview: u8,
}

struct FakeService {
    statuses: watch::Sender<Vec<McpStatusEvent>>,
    calls: Mutex<Vec<Call>>,
    ledger: Mutex<Ledger>,
    keychain: Mutex<BTreeMap<String, String>>,
    keychain_error: Mutex<Option<McpErrorKind>>,
}

impl FakeService {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            statuses: watch::channel(Vec::new()).0,
            calls: Mutex::default(),
            ledger: Mutex::default(),
            keychain: Mutex::default(),
            keychain_error: Mutex::default(),
        })
    }

    fn record(&self, call: Call) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self, matches: impl Fn(&Call) -> bool) -> usize {
        self.calls().iter().filter(|call| matches(call)).count()
    }

    fn publish(&self, root: &Path, statuses: &[(&str, McpServerStatus, Option<McpErrorKind>)]) {
        let root = std::fs::canonicalize(root).expect("canonical root");
        self.statuses.send_replace(
            statuses
                .iter()
                .map(|(id, status, reason)| McpStatusEvent {
                    scope: ApprovalScope {
                        canonical_root: root.clone(),
                        server_id: (*id).to_owned(),
                    },
                    status: *status,
                    reason: *reason,
                })
                .collect(),
        );
    }

    fn view(&self, root: &Path) -> Result<McpSettingsView, McpErrorKind> {
        let config =
            config::mcp::load_project(root).map_err(runtime::mcp::mcp_config_error_kind)?;
        Ok(McpSettingsView {
            config,
            statuses: self.statuses.borrow().clone(),
        })
    }

    fn current_revision(&self, root: &Path) -> SourceRevision {
        config::mcp::load_project(root).expect("load").revision
    }
}

fn is_http(scope: &ApprovalScope) -> bool {
    config::mcp::load_project(&scope.canonical_root)
        .ok()
        .and_then(|config| config.servers.get(&scope.server_id)?.definition.clone())
        .is_some_and(|definition| {
            matches!(definition.transport, McpTransportDefinition::Http { .. })
        })
}

/// The approval identity depends only on the configuration revision here; the
/// real projection is the approval ledger's business, not the dialog's.
fn fingerprint(revision: SourceRevision, server: &str) -> ApprovalFingerprint {
    ApprovalFingerprint::from_projection(&[revision.0.as_slice(), server.as_bytes()].concat())
}

#[async_trait]
impl McpGuiService for FakeService {
    async fn settings(&self, canonical_root: PathBuf) -> Result<McpSettingsView, McpErrorKind> {
        self.record(Call::Settings);
        self.view(&canonical_root)
    }

    async fn save_override(
        &self,
        canonical_root: PathBuf,
        expected_revision: SourceRevision,
        mutation: McpOverrideMutation,
    ) -> Result<McpSettingsView, McpErrorKind> {
        self.record(Call::Save);
        config::mcp::save_override(&canonical_root, expected_revision, mutation)
            .map_err(runtime::mcp::mcp_config_error_kind)?;
        self.view(&canonical_root)
    }

    fn subscribe_status(&self, _canonical_root: PathBuf) -> watch::Receiver<Vec<McpStatusEvent>> {
        self.statuses.subscribe()
    }

    async fn preview_approval(
        &self,
        scope: ApprovalScope,
        expected_revision: SourceRevision,
    ) -> Result<McpApprovalPreview, McpErrorKind> {
        self.record(Call::Preview);
        if self.current_revision(&scope.canonical_root) != expected_revision {
            return Err(McpErrorKind::ConfigConflict);
        }
        let mut ledger = self.ledger.lock().unwrap();
        ledger.next_preview += 1;
        let id = ApprovalPreviewId([ledger.next_preview; 16]);
        let fingerprint = fingerprint(expected_revision, &scope.server_id);
        ledger.previews.insert(
            scope.server_id.clone(),
            (id, expected_revision, fingerprint),
        );
        let (warnings, summary, approved_ips) = if is_http(&scope) {
            // A loopback destination, as the service reports it after DNS.
            (
                vec![
                    ApprovalWarning::PrivateDestination,
                    ApprovalWarning::PlaintextHttp,
                    ApprovalWarning::SendsSecretReferences,
                ],
                McpApprovalSummary {
                    executable: None,
                    canonical_cwd: None,
                    destination: Some(McpDestinationSummary {
                        scheme: "http".into(),
                        host: "127.0.0.1".into(),
                        port: 3000,
                    }),
                    environment_keys: Vec::new(),
                    secret_references: vec!["local-token".into()],
                },
                vec![IpAddr::from([127, 0, 0, 1])],
            )
        } else {
            (
                vec![ApprovalWarning::HostProcessOutsideSandbox],
                McpApprovalSummary {
                    executable: Some(PathBuf::from("/usr/bin/node")),
                    canonical_cwd: Some(scope.canonical_root.clone()),
                    destination: None,
                    environment_keys: Vec::new(),
                    secret_references: Vec::new(),
                },
                Vec::new(),
            )
        };
        Ok(McpApprovalPreview {
            id,
            scope: scope.clone(),
            source_revision: expected_revision,
            fingerprint,
            warnings,
            previous_fingerprint: ledger.approved.get(&scope.server_id).copied(),
            summary,
            approved_ips,
        })
    }

    /// Re-reads the files like the real service: a preview of an older
    /// revision, or not the latest preview, is refused.
    async fn confirm_approval(&self, confirmation: ConfirmMcpApproval) -> Result<(), McpErrorKind> {
        self.record(Call::Confirm {
            preview_id: confirmation.preview_id,
            revision: confirmation.expected_revision,
            fingerprint: confirmation.expected_fingerprint,
        });
        let current = self.current_revision(&confirmation.scope.canonical_root);
        let mut ledger = self.ledger.lock().unwrap();
        let pending = ledger.previews.get(&confirmation.scope.server_id).copied();
        let matches = pending
            == Some((
                confirmation.preview_id,
                confirmation.expected_revision,
                confirmation.expected_fingerprint,
            ));
        if !matches || current != confirmation.expected_revision {
            return Err(McpErrorKind::StalePreview);
        }
        ledger.approved.insert(
            confirmation.scope.server_id,
            confirmation.expected_fingerprint,
        );
        Ok(())
    }

    async fn revoke_approval(&self, scope: ApprovalScope) -> Result<(), McpErrorKind> {
        self.record(Call::Revoke);
        self.ledger
            .lock()
            .unwrap()
            .approved
            .remove(&scope.server_id);
        Ok(())
    }

    async fn test_connection(
        &self,
        scope: ApprovalScope,
        _expected_revision: SourceRevision,
        _cancel: CancelContext,
    ) -> Result<McpServerStatus, McpErrorKind> {
        self.record(Call::Test);
        if self
            .ledger
            .lock()
            .unwrap()
            .approved
            .contains_key(&scope.server_id)
        {
            Ok(McpServerStatus::Ready)
        } else {
            Err(McpErrorKind::NeedsApproval)
        }
    }

    /// Keychain first, then the TOML mutation; a keychain failure writes nothing.
    async fn save_secret(&self, request: SaveMcpSecret) -> Result<McpSettingsView, McpErrorKind> {
        self.record(Call::SaveSecret {
            logical_name: request.logical_name.clone(),
        });
        if let Some(kind) = *self.keychain_error.lock().unwrap() {
            return Err(kind);
        }
        self.keychain.lock().unwrap().insert(
            format!("{}/{}", request.scope.server_id, request.logical_name),
            request.value.expose().to_owned(),
        );
        config::mcp::save_override(
            &request.scope.canonical_root,
            request.expected_revision,
            request.mutation,
        )
        .map_err(runtime::mcp::mcp_config_error_kind)?;
        self.view(&request.scope.canonical_root)
    }
}

/// Counts what reaches the conversation runtime.
struct CountingSink(Arc<AtomicUsize>);

impl CommandSink for CountingSink {
    fn submit(&mut self, _cmd: WorkbenchCommand) -> Vec<LoopEvent> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Vec::new()
    }
}

const STDIO: &str = r#"version = 1
[servers.files]
type = "stdio"
command = "node"
args = ["./server.js"]
"#;

const HTTP: &str = r#"version = 1
[servers.remote]
type = "http"
url = "https://mcp.example.com/mcp"
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    harness: HeadlessWorkbench<DemoSource>,
    service: Arc<FakeService>,
    submitted: Arc<AtomicUsize>,
}

impl Fixture {
    fn new(toml: &str) -> Self {
        Self::sized(toml, [1200.0, 1600.0])
    }

    fn sized(toml: &str, size: [f32; 2]) -> Self {
        let temp = tempfile::tempdir().expect("temp");
        let root = temp.path().join("project");
        std::fs::create_dir_all(root.join(".evorch")).expect("project");
        std::fs::write(root.join(".evorch/mcp.toml"), toml).expect("mcp.toml");
        let mut sidebar = SidebarState::default();
        let id = ProjectId::new("demo");
        sidebar.add_project(id.clone(), "demo", &root).expect("add");
        sidebar.select_project(&id).expect("select");
        let service = FakeService::new();
        let submitted = Arc::new(AtomicUsize::new(0));
        let state =
            WorkbenchState::new(DemoSource(Vec::new()), &workspace_ui::UiSettings::default())
                .expect("state")
                .with_sidebar(sidebar)
                .with_command_sink(Box::new(CountingSink(submitted.clone())))
                .with_mcp_settings_service(service.clone(), None);
        Self {
            _temp: temp,
            root,
            harness: HeadlessWorkbench::new(state, size),
            service,
            submitted,
        }
    }

    fn open_settings(&mut self) {
        self.harness
            .state_mut()
            .open_project_settings(ProjectId::new("demo"));
        self.harness.run();
        self.click(MCP_SERVERS_BUTTON_LABEL);
    }

    /// Waits for the background calls the last frame started, then redraws.
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

    fn toml(&self) -> String {
        std::fs::read_to_string(self.root.join(".evorch/mcp.toml")).expect("mcp.toml")
    }

    fn server_table(&self, id: &str) -> toml::Table {
        let document: toml::Table = self.toml().parse().expect("toml");
        document["servers"][id].as_table().expect("server").clone()
    }

    fn approval_state(&self) -> Option<McpApprovalState> {
        self.harness
            .state()
            .mcp_settings()
            .approval()
            .map(|review| review.state.clone())
    }
}

#[test]
fn approval_preview_rejects_stale_revision() {
    // Given: an unapproved stdio server and its approval preview on screen.
    let mut fixture = Fixture::new(STDIO);
    fixture.service.publish(
        &fixture.root,
        &[(
            "files",
            McpServerStatus::NeedsApproval,
            Some(McpErrorKind::NeedsApproval),
        )],
    );
    fixture.open_settings();
    assert!(fixture.harness.has_label("needs approval"));
    fixture.click("Review approval files");
    let Some(McpApprovalState::Ready(first)) = fixture.approval_state() else {
        panic!("preview: {:?}", fixture.approval_state());
    };
    // The preview says what runs and what approval does not cover.
    assert!(fixture.harness.has_label(OUTSIDE_SANDBOX_WARNING));
    assert!(fixture.harness.has_label(UNVERIFIED_CONTENT_NOTE));
    assert!(fixture.harness.has_label("1. ./server.js"));
    assert!(fixture.harness.has_label("/usr/bin/node"));
    // When: the launch settings change on disk before the user approves.
    std::fs::write(
        fixture.root.join(".evorch/mcp.toml"),
        STDIO.replace("[\"./server.js\"]", "[\"./server.js\", \"--evil\"]"),
    )
    .expect("external edit");
    fixture.click("Approve files");
    // Then: the confirmation carried the preview's identity, the service
    // refused it as stale, the dialog says so and nothing is approved.
    assert_eq!(
        fixture
            .service
            .count(|call| matches!(call, Call::Confirm { .. })),
        1
    );
    let confirmed = fixture
        .service
        .calls()
        .into_iter()
        .find(|call| matches!(call, Call::Confirm { .. }))
        .expect("confirmation");
    assert_eq!(
        confirmed,
        Call::Confirm {
            preview_id: first.id,
            revision: first.source_revision,
            fingerprint: first.fingerprint,
        }
    );
    assert!(matches!(
        fixture.approval_state(),
        Some(McpApprovalState::Stale)
    ));
    assert!(fixture.harness.has_label(STALE_PREVIEW_TEXT));
    assert!(fixture.service.ledger.lock().unwrap().approved.is_empty());
    // When: reviewing again and approving the current settings.
    fixture.click(REVIEW_AGAIN_LABEL);
    let Some(McpApprovalState::Ready(second)) = fixture.approval_state() else {
        panic!("fresh preview: {:?}", fixture.approval_state());
    };
    assert_ne!(second.source_revision, first.source_revision);
    assert!(fixture.harness.has_label("2. --evil"));
    fixture.click(&format!("{APPROVE_LABEL} files"));
    // Then: exactly the fresh preview is approved, for the next new run.
    assert!(fixture.approval_state().is_none());
    assert_eq!(
        fixture.service.ledger.lock().unwrap().approved.get("files"),
        Some(&second.fingerprint)
    );
    assert!(fixture.harness.has_label(NEXT_RUN_NOTICE));
}

#[test]
fn unapproved_connection_test_is_refused() {
    // Given: one server reported unapproved and one with no report yet.
    let toml = format!("{STDIO}{}", HTTP.trim_start_matches("version = 1\n"));
    let mut fixture = Fixture::new(&toml);
    fixture.service.publish(
        &fixture.root,
        &[("files", McpServerStatus::NeedsApproval, None)],
    );
    fixture.open_settings();
    // Then: testing the unapproved server is not offered, and asking anyway
    // is refused without calling the service.
    assert_eq!(
        fixture.harness.label_enabled("Test connection files"),
        Some(false)
    );
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .apply_action(McpSettingsAction::TestConnection("files".into()));
    fixture.settle();
    assert_eq!(
        fixture.harness.state().mcp_settings().row_note("files"),
        Some(&McpRowNote::TestRefused)
    );
    assert!(fixture.harness.has_label(TEST_REFUSED_TEXT));
    assert_eq!(fixture.service.count(|call| *call == Call::Test), 0);
    // When: testing the server whose approval is unknown.
    fixture.click("Test connection remote");
    // Then: the service's refusal is shown the same way.
    assert_eq!(fixture.service.count(|call| *call == Call::Test), 1);
    assert_eq!(
        fixture.harness.state().mcp_settings().row_note("remote"),
        Some(&McpRowNote::TestRefused)
    );
    // When: approving it and testing again.
    fixture.click("Review approval remote");
    fixture.click(&format!("{APPROVE_LABEL} remote"));
    fixture.click("Test connection remote");
    // Then: only the safe state is reported.
    assert_eq!(
        fixture.harness.state().mcp_settings().row_note("remote"),
        Some(&McpRowNote::Tested(McpServerStatus::Ready))
    );
    assert!(fixture.harness.has_label("Connection test: ready."));
}

#[test]
fn token_input_persists_only_keychain_reference() {
    const VALUE: &str = "sk-live-0123456789";
    let mut fixture = Fixture::new(HTTP);
    fixture.open_settings();
    // Given: the bearer token switched to a keychain entry named remote-token.
    fixture.click("Edit remote");
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .editor_mut()
        .expect("editor")
        .token = Some(DraftValue::keychain("remote-token"));
    fixture.harness.run();
    // When: typing the value into the masked input and storing it.
    fixture.click("Set value remote-token");
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .secret_input_mut()
        .expect("secret input")
        .value = VALUE.into();
    fixture.harness.run();
    assert!(!fixture.harness.has_label(VALUE), "the value is masked");
    assert!(fixture.harness.has_label("Secret value for remote-token"));
    assert!(
        !fixture
            .harness
            .label_value("Secret value for remote-token")
            .is_some_and(|value| value.contains(VALUE))
    );
    let debug = format!(
        "{:?}",
        fixture.harness.state().mcp_settings().secret_input()
    );
    assert!(!debug.contains(VALUE), "{debug}");
    fixture.click(STORE_SECRET_LABEL);
    // Then: the value went only to the keychain; mcp.toml names the entry.
    assert_eq!(
        fixture.service.calls().last(),
        Some(&Call::SaveSecret {
            logical_name: "remote-token".into()
        })
    );
    assert_eq!(
        fixture
            .service
            .keychain
            .lock()
            .unwrap()
            .get("remote/remote-token")
            .map(String::as_str),
        Some(VALUE)
    );
    let remote = fixture.server_table("remote");
    assert_eq!(
        remote["token"]["keychain"].as_str(),
        Some("remote-token"),
        "{remote:?}"
    );
    assert!(!fixture.toml().contains(VALUE));
    let state = fixture.harness.state().mcp_settings();
    assert_eq!(state.notice(), Some(&McpNotice::SecretStored));
    assert!(state.secret_input().is_none());
    assert!(state.editor().is_none());
    assert!(!fixture.harness.has_label(VALUE));
    // When: editing again, the stored value is not shown and can be replaced.
    fixture.click("Edit remote");
    assert!(fixture.harness.has_label("Replace value remote-token"));
    assert!(!fixture.harness.has_label(VALUE));
    // When: the bearer token is typed as plain text instead, it is masked too.
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .editor_mut()
        .expect("editor")
        .token = Some(DraftValue::text("sk-plain-typed"));
    fixture.harness.run();
    assert!(!fixture.harness.has_label("sk-plain-typed"));
    assert!(fixture.harness.has_label("Bearer token value"));
    assert!(
        !fixture
            .harness
            .label_value("Bearer token value")
            .is_some_and(|value| value.contains("sk-plain-typed"))
    );
}

#[test]
fn secret_backend_errors_have_safe_status() {
    // Given: servers whose keychain secrets failed in four distinct ways.
    let ids = ["a-unavailable", "b-locked", "c-denied", "d-missing"];
    let kinds = [
        McpErrorKind::SecretUnavailable,
        McpErrorKind::SecretLocked,
        McpErrorKind::SecretDenied,
        McpErrorKind::SecretMissing,
    ];
    let mut toml = String::from("version = 1\n");
    for id in ids {
        toml.push_str(&format!(
            "[servers.{id}]\ntype = \"http\"\nurl = \"https://mcp.example.com/{id}\"\ntoken = {{ keychain = \"{id}-token\" }}\n"
        ));
    }
    let mut fixture = Fixture::new(&toml);
    let statuses: Vec<_> = ids
        .iter()
        .zip(kinds)
        .map(|(id, kind)| (*id, McpServerStatus::SecretUnavailable, Some(kind)))
        .collect();
    fixture.service.publish(&fixture.root, &statuses);
    fixture.open_settings();
    // Then: each row explains its own cause.
    for kind in kinds {
        assert!(
            fixture.harness.has_label(&secret_error_text(kind)),
            "{kind:?}"
        );
    }
    // When: storing a value while the keychain refuses, in each way.
    let before = fixture.toml();
    for kind in [
        McpErrorKind::SecretUnavailable,
        McpErrorKind::SecretLocked,
        McpErrorKind::SecretDenied,
    ] {
        *fixture.service.keychain_error.lock().unwrap() = Some(kind);
        fixture.click("Edit a-unavailable");
        fixture.click("Replace value a-unavailable-token");
        fixture
            .harness
            .state_mut()
            .mcp_settings_mut()
            .secret_input_mut()
            .expect("secret input")
            .value = "typed-secret".into();
        fixture.harness.run();
        fixture.click(STORE_SECRET_LABEL);
        // Then: the failure is explained, nothing falls back to a file and
        // the typed value is gone from the form.
        let state = fixture.harness.state().mcp_settings();
        assert_eq!(
            state.notice(),
            Some(&McpNotice::Failed(secret_error_text(kind)))
        );
        assert!(state.secret_input().expect("input").value.is_empty());
        assert_eq!(fixture.toml(), before);
        assert!(fixture.service.keychain.lock().unwrap().is_empty());
        fixture
            .harness
            .state_mut()
            .mcp_settings_mut()
            .apply_action(McpSettingsAction::CancelEdit);
        fixture.harness.run();
    }
}

#[test]
fn approval_does_not_mutate_current_conversation_tools() {
    // Given: settings open on an unapproved server.
    let mut fixture = Fixture::new(STDIO);
    fixture.service.publish(
        &fixture.root,
        &[("files", McpServerStatus::NeedsApproval, None)],
    );
    fixture.open_settings();
    let calls_before = fixture.service.calls().len();
    // When: approving it.
    fixture.click("Review approval files");
    fixture.click(&format!("{APPROVE_LABEL} files"));
    // Then: the dialog says it applies from the next new run and asks nothing
    // else: no conversation command, no reload, connection or other call.
    assert!(fixture.harness.has_label(NEXT_RUN_NOTICE));
    assert_eq!(fixture.submitted.load(Ordering::SeqCst), 0);
    assert!(fixture.harness.state().issued().is_empty());
    let after: Vec<_> = fixture.service.calls()[calls_before..].to_vec();
    assert_eq!(after.len(), 2, "{after:?}");
    assert_eq!(after[0], Call::Preview);
    assert!(matches!(after[1], Call::Confirm { .. }));
    // When: revoking it again.
    fixture.click("Revoke approval files");
    // Then: likewise only the revocation is requested.
    assert_eq!(fixture.service.calls().last(), Some(&Call::Revoke));
    assert_eq!(fixture.submitted.load(Ordering::SeqCst), 0);
    assert!(fixture.harness.has_label("needs approval"));
}

#[test]
fn skipped_server_notice_opens_approval() {
    // Given: the workbench is open and follows the project's MCP status.
    let mut fixture = Fixture::new(STDIO);
    fixture.harness.run();
    fixture
        .harness
        .state_mut()
        .mcp_settings_mut()
        .wait_project_watches();
    fixture.harness.run();
    assert!(!fixture.harness.has_label(NOTICE_REVIEW_LABEL));
    // When: a new run skips the unapproved server.
    fixture.service.publish(
        &fixture.root,
        &[(
            "files",
            McpServerStatus::NeedsApproval,
            Some(McpErrorKind::NeedsApproval),
        )],
    );
    fixture.harness.run();
    // Then: a non-blocking notice offers the approval.
    assert_eq!(
        fixture
            .harness
            .state()
            .mcp_settings()
            .approval_notices()
            .len(),
        1
    );
    assert!(fixture.harness.has_label("MCP server files was skipped"));
    assert!(matches!(
        fixture.harness.state().project_dialog(),
        ProjectDialog::Closed
    ));
    // When: following it.
    fixture.click(&format!("{NOTICE_REVIEW_LABEL} files"));
    // Then: the project's MCP servers open on the approval preview.
    assert!(matches!(
        fixture.harness.state().project_dialog(),
        ProjectDialog::Settings { .. }
    ));
    assert!(matches!(
        fixture.approval_state(),
        Some(McpApprovalState::Ready(_))
    ));
    assert!(
        fixture
            .harness
            .state()
            .mcp_settings()
            .approval_notices()
            .is_empty()
    );
    fixture.click(&format!("{APPROVE_LABEL} files"));
    assert!(fixture.harness.has_label(NEXT_RUN_NOTICE));
    assert_eq!(fixture.submitted.load(Ordering::SeqCst), 0);
}

#[test]
fn draft_debug_redacts_values() {
    let mut draft = McpServerDraft::new_server();
    draft.id = "files".into();
    draft.command = "/opt/cmd-with-sk-cmd".into();
    draft.args = vec!["--token=sk-arg".into()];
    draft.cwd = "./sk-cwd".into();
    draft.transport = DraftTransport::Http;
    draft.url = "https://mcp.example.com/mcp?key=sk-query".into();
    draft.token = Some(DraftValue::text("sk-token"));
    let debug = format!("{draft:?}");
    for secret in ["sk-cmd", "sk-arg", "sk-cwd", "sk-query", "sk-token"] {
        assert!(!debug.contains(secret), "{secret} in {debug}");
    }
    assert!(debug.contains("files"));
    // Keychain references are names, not values, and may be listed.
    draft.token = Some(DraftValue::keychain("remote-token"));
    assert_eq!(
        draft.keychain_references(),
        vec![(SecretTarget::Token, "remote-token".to_owned())]
    );
}

#[test]
fn approval_preview_hides_literal_secrets() {
    const TOKEN: &str = "sk-token-sentinel";
    const AUTH: &str = "Bearer sk-auth-sentinel";
    const ENV: &str = "sk-env-sentinel";
    let hidden = |text: &str| format!("[hidden text, {} characters]", text.chars().count());
    let toml = format!(
        r#"version = 1
[servers.files]
type = "stdio"
command = "node"
args = ["./server.js"]
[servers.files.env]
API_KEY = "{ENV}"
MODE = "dev-visible"
REF = "${{FILES_TOKEN:-fallback}}"
[servers.bearer]
type = "http"
url = "https://mcp.example.com/mcp"
token = "{TOKEN}"
[servers.header]
type = "http"
url = "https://mcp.example.com/mcp"
[servers.header.headers]
Authorization = "{AUTH}"
X-Tenant = "tenant-visible"
"#
    );
    let mut fixture = Fixture::new(&toml);
    fixture.open_settings();
    let review = |fixture: &mut Fixture, id: &str| {
        fixture
            .harness
            .state_mut()
            .mcp_settings_mut()
            .apply_action(McpSettingsAction::CancelApproval);
        fixture.harness.run();
        fixture.click(&format!("Review approval {id}"));
        assert!(matches!(
            fixture.approval_state(),
            Some(McpApprovalState::Ready(_))
        ));
    };
    // A credential-like environment variable shows only its length; other
    // plain values and ${VAR} references stay readable for the decision.
    review(&mut fixture, "files");
    assert!(
        fixture
            .harness
            .has_label(&format!("API_KEY = {}", hidden(ENV)))
    );
    assert!(!fixture.harness.has_label(&format!("API_KEY = {ENV}")));
    assert!(fixture.harness.has_label("MODE = dev-visible"));
    assert!(fixture.harness.has_label("REF = ${FILES_TOKEN:-fallback}"));
    // A plain bearer token is always hidden.
    review(&mut fixture, "bearer");
    assert!(fixture.harness.has_label(&hidden(TOKEN)));
    assert!(!fixture.harness.has_label(TOKEN));
    // So is an Authorization header; other headers are shown.
    review(&mut fixture, "header");
    assert!(
        fixture
            .harness
            .has_label(&format!("Authorization = {}", hidden(AUTH)))
    );
    assert!(
        !fixture
            .harness
            .has_label(&format!("Authorization = {AUTH}"))
    );
    assert!(fixture.harness.has_label("X-Tenant = tenant-visible"));
}
