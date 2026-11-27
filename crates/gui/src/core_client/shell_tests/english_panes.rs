use super::*;
use std::collections::{BTreeMap, HashMap};

use egui_kittest::kittest::NodeT;
use workbench_client::renderer::Frame;
use workbench_protocol::{
    commands::{Command, CommandReply, CommandResult},
    handshake::Capability,
    ids::RestartId,
    namespaces::{
        artifacts::{
            ArtifactMetadata, ArtifactRef, ArtifactsCommand, ArtifactsQuery, ArtifactsReply,
            ArtifactsResult,
        },
        browser::{
            BrowserCommand, BrowserQuery, BrowserReply, BrowserResult, RendererId,
            RendererOperation, RendererOperationState, RendererOutput, RendererOwnerKind,
            RendererSessionRef, RendererTab, RendererTabRef, data::FrameStart,
        },
        lifecycle::{
            AdminId, LifecycleQuery, LifecycleReply, RestartPhase,
            restart::{
                AdminAuditDiagnostics, InterruptBinding, RestartBlocker, RestartBlockerKind,
                RestartManagementStatus, RestartQuery, RestartReply, RestartStatus, RestartTarget,
                RestartTargets, ShellBlocker, ShellBlockers,
            },
        },
        mcp::{
            McpCommand, McpErrorCode, McpOperationId, McpOperationKind, McpOperationState,
            McpOperationView, McpOrigin, McpOutcome, McpQuery, McpReply, McpResult, McpRevision,
            McpSecretState, McpServerDefinition, McpServerState, McpServerStatus, McpServerView,
            McpSettings, McpTemplate, McpTemplatePart, McpTransport, McpValueSource,
        },
    },
};

use crate::core_client::features::data::{DataAction, DataNotice, DataPayload, DataRequest};

fn has_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c, '\u{3000}'..='\u{30ff}' | '\u{3400}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
    })
}

fn assert_english(screen: &Screen) {
    for node in screen.harness.root().children_recursive() {
        let node = node.accesskit_node();
        for text in [node.label(), node.value()].into_iter().flatten() {
            let text = text.to_string();
            assert!(!has_cjk(&text), "{text}");
        }
    }
    let mut painted = Vec::new();
    for shape in &screen.harness.output().shapes {
        collect_text(&shape.shape, &mut painted);
    }
    for text in painted {
        assert!(!has_cjk(&text), "{text}");
    }
}

fn collect_text(shape: &egui::Shape, out: &mut Vec<String>) {
    match shape {
        egui::Shape::Text(text) => out.push(text.galley.text().to_owned()),
        egui::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_text(shape, out);
            }
        }
        _ => {}
    }
}

impl Screen {
    // A fixed number of frames: some panes poll, so the queue never runs dry.
    fn serve(&mut self, core: &mut dyn FnMut(&mut Shared, Action)) {
        for _ in 0..24 {
            self.harness.run_steps(1);
            while let Ok(action) = self.actions.try_recv() {
                let mut shared = self.shared.lock().unwrap();
                match action {
                    Action::Feature(FeatureAction {
                        feature: FeatureId::Workspace,
                        tag,
                        request: FeatureRequest::Query(Query::Workspace(query)),
                    }) => {
                        let reply = FeatureReply::Query(Ok(QueryReply::Workspace(answer(&query))));
                        shared.features.reply(FeatureId::Workspace, tag, reply);
                    }
                    other => core(&mut shared, other),
                }
            }
        }
        self.harness
            .event(egui::Event::PointerMoved(egui::pos2(1.0, 899.0)));
        self.harness.run_steps(2);
    }

    fn open_feature(&mut self, feature: FeatureId, core: &mut dyn FnMut(&mut Shared, Action)) {
        let asked = self.settle();
        self.show_feature(feature);
        for action in asked {
            core(&mut self.shared.lock().unwrap(), action);
        }
        self.serve(core);
    }

    fn show_feature(&mut self, feature: FeatureId) {
        let tab = CoreTab::Feature(feature);
        let dock = &mut self.harness.state_mut().dock;
        shell::focus(dock, tab);
        let path = dock
            .find_tab(&tab)
            .expect("the tab is in the dock")
            .node_path();
        if let Some(parent) = path.node.parent() {
            let top = path.node.is_left();
            if let egui_dock::Node::Vertical(split) =
                &mut dock[egui_dock::NodePath::new(path.surface, parent)]
            {
                split.fraction = if top { 0.85 } else { 0.15 };
            }
        }
    }
}

const PAGE: &str = "<!doctype html>\n<html lang=\"en\">\n<head>\n  <meta charset=\"utf-8\">\n  <title>Login redirect report</title>\n</head>\n<body>\n  <h1>Login redirect</h1>\n  <p>The handler now honours <code>next</code>.</p>\n  <ul>\n    <li>auth::redirect — 12 passed</li>\n    <li>auth::session — 8 passed</li>\n  </ul>\n</body>\n</html>\n";

fn reference(n: u8) -> ArtifactRef {
    ArtifactRef {
        project: ProjectId::new("proj-1").unwrap(),
        thread: ThreadId::new("thr-1").unwrap(),
        artifact_id: format!("artifact-{}", format!("{n:02x}").repeat(16)),
        digest: format!("{n:02x}").repeat(32),
    }
}

fn artifacts() -> Vec<ArtifactMetadata> {
    [
        (1, "redirect-report.html", "text/html", PAGE.len() as u64),
        (2, "login-before.png", "image/png", 245_760),
        (3, "test-summary.txt", "text/plain", 2_048),
    ]
    .into_iter()
    .map(|(n, title, media, len)| ArtifactMetadata {
        reference: reference(n),
        title: title.into(),
        media_type: media.into(),
        byte_len: len,
    })
    .collect()
}

fn answer_artifacts(shared: &mut Shared, action: &Action) -> bool {
    let Action::Feature(FeatureAction {
        feature: FeatureId::Artifacts,
        tag,
        request: FeatureRequest::Query(Query::Artifacts(query)),
    }) = action
    else {
        return false;
    };
    let reply = match query {
        ArtifactsQuery::List { .. } => ArtifactsReply::List {
            artifacts: artifacts(),
            next: Some("cursor-2".into()),
        },
        ArtifactsQuery::Get { artifact } => ArtifactsReply::Metadata(
            artifacts()
                .into_iter()
                .find(|meta| &meta.reference == artifact)
                .expect("a listed artifact"),
        ),
        ArtifactsQuery::ReadContent { artifact, .. } => ArtifactsReply::Content {
            artifact: artifact.clone(),
            offset: 0,
            text: PAGE.into(),
            eof: true,
        },
    };
    let reply = FeatureReply::Query(Ok(QueryReply::Artifacts(reply)));
    shared.features.reply(FeatureId::Artifacts, *tag, reply);
    true
}

fn open_the_report(screen: &mut Screen) {
    screen.open_feature(FeatureId::Artifacts, &mut |shared, action| {
        answer_artifacts(shared, &action);
    });
    screen
        .harness
        .get_by_label(&format!(
            "redirect-report.html  (text/html, {} B)",
            PAGE.len()
        ))
        .click();
    screen.serve(&mut |shared, action| {
        answer_artifacts(shared, &action);
    });
}

// Given: thr-1 with an HTML report, a screenshot and a text file, and more on the next page
// When: the Artifacts tab lists them and the report is opened
// Then: the header, the rows, the paging button and the opened source read in English
#[test]
fn the_artifacts_tab_lists_and_opens_a_report_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    open_the_report(&mut screen);
    screen.harness.get_by_label(&crate::theme::icons::with_icon(
        crate::theme::icons::IMAGES,
        "Artifacts (3)",
    ));
    for label in [
        "Reload artifacts",
        "Load more artifacts",
        "Open preview",
        "Showing the HTML source; it is not run here.",
        "login-before.png  (image/png, 245760 B)",
    ] {
        screen.harness.get_by_label(label);
    }
    assert_english(&screen);
    screen.capture("artifacts");
}

#[derive(Default)]
struct Renderer {
    serial: u64,
    states: HashMap<String, RendererOperationState>,
    tabs: Vec<RendererTab>,
}

impl Renderer {
    fn id(&mut self) -> RendererId {
        self.serial += 1;
        RendererId::new(format!("{:032x}", self.serial)).unwrap()
    }

    fn output(&mut self, command: &Command) -> RendererOutput {
        match command {
            Command::Browser(BrowserCommand::CreateRendererSession {}) => RendererOutput::Session {
                session: RendererSessionRef {
                    core_epoch: 7,
                    id: self.id(),
                },
                resume_token: self.id(),
            },
            Command::Artifacts(ArtifactsCommand::OpenPreview {
                artifact,
                session,
                viewport,
            }) => {
                let tab = RendererTab {
                    reference: RendererTabRef {
                        core_epoch: 7,
                        generation: 1,
                        id: self.id(),
                    },
                    session: session.clone(),
                    owner: RendererOwnerKind::ClientPreview,
                    artifact: artifact.clone(),
                    viewport: *viewport,
                    fps: 30,
                    streaming: false,
                };
                self.tabs.push(tab.clone());
                RendererOutput::Tab(Box::new(tab))
            }
            Command::Browser(BrowserCommand::SubscribeRendererFrames { tab }) => {
                RendererOutput::Subscription {
                    subscription: self.id(),
                    token: self.id(),
                    tab: tab.clone(),
                }
            }
            Command::Browser(BrowserCommand::ResizeRendererTab { tab, viewport }) => {
                if let Some(t) = self.tabs.iter_mut().find(|t| &t.reference == tab) {
                    t.viewport = *viewport;
                }
                RendererOutput::Done
            }
            Command::Browser(BrowserCommand::TakeRendererNotices { .. }) => {
                RendererOutput::Notices {
                    blocked: vec!["https://tracker.example/pixel.gif".into()],
                    dialogs: vec!["alert: Saved the redirect target".into()],
                }
            }
            Command::Browser(BrowserCommand::RendererCdp { .. }) => RendererOutput::Cdp {
                result: json!({
                    "cssLayoutViewport": {"pageX": 0, "pageY": 0, "clientWidth": 960, "clientHeight": 540},
                    "cssContentSize": {"x": 0, "y": 0, "width": 960, "height": 812},
                }),
            },
            _ => RendererOutput::Done,
        }
    }

    fn serve(&mut self, shared: &mut Shared, action: Action) {
        if answer_artifacts(shared, &action) {
            return;
        }
        match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Browser,
                tag,
                request,
            }) => {
                let reply = match request {
                    FeatureRequest::Command(command) => {
                        let output = self.output(&command);
                        let operation_id = self.id();
                        self.states.insert(
                            operation_id.as_str().to_owned(),
                            RendererOperationState::Completed { output },
                        );
                        let result = match command {
                            Command::Artifacts(_) => {
                                CommandResult::Artifacts(ArtifactsResult::Accepted { operation_id })
                            }
                            _ => CommandResult::Browser(BrowserResult::Accepted { operation_id }),
                        };
                        FeatureReply::Command(Ok(CommandReply::with_result(result)))
                    }
                    FeatureRequest::Query(Query::Browser(BrowserQuery::GetRendererOperation {
                        operation_id,
                    })) => FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::Operation(
                        RendererOperation {
                            state: self.states[operation_id.as_str()].clone(),
                            operation_id,
                        },
                    )))),
                    FeatureRequest::Query(Query::Browser(BrowserQuery::ListRendererTabs {
                        ..
                    })) => FeatureReply::Query(Ok(QueryReply::Browser(BrowserReply::Tabs {
                        tabs: self.tabs.clone(),
                        more: false,
                    }))),
                    _ => return,
                };
                shared.features.reply(FeatureId::Browser, tag, reply);
            }
            Action::Data(action) => self.stream(shared, action),
            _ => {}
        }
    }

    fn stream(&mut self, shared: &mut Shared, action: DataAction) {
        let (feature, tag) = (action.feature, action.tag);
        let DataRequest::Start(source) = &action.request else {
            shared.features.data.remove(feature, tag);
            shared
                .features
                .data_notice(feature, tag, DataNotice::Cancelled);
            return;
        };
        let key = shared
            .features
            .data
            .claim(&action, source.clone())
            .expect("the inbox has room");
        shared
            .features
            .data_notice(feature, tag, DataNotice::Started);
        let tab = self.tabs.last().expect("an open tab");
        let frame = page_frame(tab);
        assert!(shared.features.data.publish(key, DataPayload::Frame(frame)));
        shared.features.data_notice(feature, tag, DataNotice::Ready);
    }
}

fn page_frame(tab: &RendererTab) -> Frame {
    let (w, h) = (tab.viewport.width, tab.viewport.height);
    let mut bgra = vec![0xff_u8; (w * h * 4) as usize];
    let mut fill = |x0: u32, y0: u32, x1: u32, y1: u32, [r, g, b]: [u8; 3]| {
        for y in y0.min(h)..y1.min(h) {
            for x in x0.min(w)..x1.min(w) {
                let at = ((y * w + x) * 4) as usize;
                bgra[at..at + 4].copy_from_slice(&[b, g, r, 0xff]);
            }
        }
    };
    fill(0, 0, w, 48, [0x1f, 0x29, 0x37]);
    fill(24, 16, 140, 32, [0xe5, 0xe7, 0xeb]);
    fill(32, 80, w * 2 / 5, 104, [0x11, 0x18, 0x27]);
    for line in 0..4 {
        let y = 128 + line * 22;
        fill(32, y, w * (9 - line) / 12, y + 10, [0x9c, 0xa3, 0xaf]);
    }
    fill(32, 230, w / 2, 300, [0xdc, 0xfc, 0xe7]);
    fill(44, 250, w / 3, 262, [0x16, 0x65, 0x34]);
    fill(44, 274, w / 4, 284, [0x16, 0x65, 0x34]);
    Frame {
        start: FrameStart {
            core_epoch: tab.reference.core_epoch,
            generation: tab.reference.generation,
            tab: tab.reference.id.clone(),
            sequence: 1,
            width: w,
            height: h,
            stride: w * 4,
            total_bytes: u64::from(w * h * 4),
        },
        bgra,
    }
}

// Given: the HTML report of thr-1 opened in Artifacts
// When: its preview is opened, streams a frame, and the page notices and its layout are read
// Then: the Browser tab, its toolbar and the results read in English
#[test]
fn the_browser_tab_previews_a_report_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    open_the_report(&mut screen);
    let mut renderer = Renderer::default();
    screen.harness.get_by_label("Open preview").click();
    screen.serve(&mut |shared, action| renderer.serve(shared, action));
    screen.harness.get_by_label("Notices").click();
    screen.serve(&mut |shared, action| renderer.serve(shared, action));
    screen.harness.get_by_label("Inspect").click();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Layout").click();
    screen.serve(&mut |shared, action| renderer.serve(shared, action));
    let id = reference(1).artifact_id;
    for label in [
        "Capture".to_owned(),
        "Notices".to_owned(),
        "Inspect".to_owned(),
        "Page notices".to_owned(),
        "Blocked: https://tracker.example/pixel.gif".to_owned(),
        "Dialog: alert: Saved the redirect target".to_owned(),
        "Inspection: Layout".to_owned(),
        format!("Close {id}"),
    ] {
        screen.harness.get_by_label(&label);
    }
    assert!(
        screen
            .harness
            .query_by_label_contains("Unexpected reply")
            .is_none()
    );
    assert_english(&screen);
    screen.capture("browser");
}

fn lifecycle(reply: RestartReply) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Lifecycle(LifecycleReply::Restart(reply))))
}

fn answer_restart(shared: &mut Shared, action: Action) {
    let Action::Feature(FeatureAction {
        feature: FeatureId::Restart,
        tag,
        request: FeatureRequest::Query(Query::Lifecycle(LifecycleQuery::Restart(query))),
    }) = action
    else {
        return;
    };
    let admin = |text: &str| AdminId::new(text).unwrap();
    let reply = match query {
        RestartQuery::Targets => RestartReply::Targets(RestartTargets {
            core_epoch: InstanceEpoch(7),
            revision: Revision(1),
            targets: vec![RestartTarget {
                id: admin("current"),
                revision: Revision(11),
            }],
            truncated: false,
        }),
        RestartQuery::Status => RestartReply::Status(RestartManagementStatus {
            audit: AdminAuditDiagnostics::default(),
            core_epoch: InstanceEpoch(7),
            status: RestartStatus {
                restart_id: Some(RestartId::new("r1").unwrap()),
                phase: RestartPhase::WaitingSafePoint,
                revision: 4,
                blockers: vec![RestartBlocker {
                    run: RunId::new("run-1").unwrap(),
                    kind: RestartBlockerKind::ApprovalPending,
                }],
                blocker_total: 1,
                blocker_omitted: 0,
                active_producers: 1,
                cancel_reason: None,
            },
        }),
        RestartQuery::ShellBlockers => RestartReply::ShellBlockers(ShellBlockers {
            blockers: vec![ShellBlocker {
                run: RunId::new("run-6").unwrap(),
                teardown_confirmed: false,
                observed: false,
                binding: InterruptBinding {
                    restart_id: admin("r1"),
                    core_epoch: InstanceEpoch(7),
                    connection_epoch: InstanceEpoch(1),
                    run_incarnation: admin("inc"),
                    blocker_identity: admin("blk"),
                    revision: Revision(4),
                    phase: RestartPhase::WaitingSafePoint,
                },
            }],
            total: Revision(1),
            omitted: Revision(0),
        }),
    };
    shared
        .features
        .reply(FeatureId::Restart, tag, lifecycle(reply));
}

// Given: a core that offers safe restart, waiting for a safe point behind an approval
//        and a running shell
// When: the Restart tab reads it and the shell's interrupt is asked for
// Then: the status, the approval warning, the shell row and the confirmation read in English
#[test]
fn the_restart_tab_waits_for_a_safe_point_in_english() {
    let local = crate::core_client::local::LocalPanes::new(
        Arc::new(crate::diff::FixtureDiffSource::empty()),
        None,
    );
    let mut screen = screen_from(egui::vec2(1440.0, 900.0), local, |shared| {
        shared.capabilities.push(Capability::SafeRestart);
        shared.features.begin_session(&[], Vec::new());
    });
    screen.open_feature(FeatureId::Restart, &mut |shared, action| {
        if !answer_artifacts(shared, &action) {
            answer_restart(shared, action);
        }
    });
    for label in [
        "Status: Waiting for a safe point (revision 4)",
        "Shells holding up the restart:",
        "run run-6",
        "Restart core…",
        "Cancel restart",
    ] {
        screen.harness.get_by_label(label);
    }
    screen
        .harness
        .get_by_label_contains("Run run-1 is waiting for approval");
    screen.harness.get_by_label("Interrupt this shell…").click();
    screen.serve(&mut answer_restart);
    screen
        .harness
        .get_by_label_contains("Interrupt the shell of run run-6");
    screen.harness.get_by_label("Interrupt");
    screen.harness.get_by_label("Cancel");
    assert_english(&screen);
    screen.capture("restart");
}

fn revision() -> McpRevision {
    McpRevision::new("a".repeat(64)).unwrap()
}

fn literal(value: &str) -> McpTemplate {
    McpTemplate(vec![McpTemplatePart::Literal {
        value: value.into(),
    }])
}

fn server(
    id: &str,
    status: McpServerStatus,
    reason: Option<McpErrorCode>,
    secret: Option<McpSecretState>,
) -> McpServerView {
    let transport = if secret.is_some() {
        McpTransport::Http {
            url: literal("https://mcp.example/github"),
            headers: BTreeMap::new(),
            token: Some(McpValueSource::Keychain {
                name: "token".into(),
            }),
        }
    } else {
        McpTransport::Stdio {
            command: literal("npx"),
            args: vec![literal(id)],
            cwd: None,
            env: BTreeMap::new(),
        }
    };
    McpServerView {
        server_id: id.into(),
        origin: McpOrigin::ProjectToml,
        imported_exists: false,
        tombstone: false,
        definition: Some(McpServerDefinition {
            enabled: status != McpServerStatus::Disabled,
            roles: vec!["orchestrator".into()],
            transport,
            startup_timeout_ms: 10_000,
            request_timeout_ms: 30_000,
        }),
        diagnostics: vec![],
        state: McpServerState { status, reason },
        secret_references: secret.iter().map(|_| "token".to_owned()).collect(),
        secret_states: secret
            .into_iter()
            .map(|state| ("token".to_owned(), state))
            .collect(),
    }
}

fn mcp_settings() -> McpSettings {
    McpSettings {
        project: ProjectId::new("proj-1").unwrap(),
        revision: revision(),
        preparation_blocked: false,
        diagnostics: vec![],
        servers: vec![
            server("docs", McpServerStatus::Ready, None, None),
            server(
                "github",
                McpServerStatus::NeedsApproval,
                Some(McpErrorCode::ApprovalRevoked),
                Some(McpSecretState::Present),
            ),
            server("search", McpServerStatus::Connecting, None, None),
            server(
                "browser-tools",
                McpServerStatus::ProcessLimitReached,
                None,
                None,
            ),
            server("analytics", McpServerStatus::Disabled, None, None),
        ],
    }
}

fn answer_mcp(shared: &mut Shared, action: Action) {
    let Action::Feature(FeatureAction {
        feature: FeatureId::Mcp,
        tag,
        request,
    }) = action
    else {
        return;
    };
    let finished =
        |operation_id: &McpOperationId, kind, server: Option<&str>, state| McpOperationView {
            operation_id: operation_id.clone(),
            project: ProjectId::new("proj-1").unwrap(),
            kind,
            server_id: server.map(str::to_owned),
            state,
        };
    let reply = match request {
        FeatureRequest::Query(Query::Mcp(McpQuery::GetSettings { .. })) => FeatureReply::Query(Ok(
            QueryReply::Mcp(McpReply::Settings(Box::new(mcp_settings()))),
        )),
        FeatureRequest::Command(Command::Mcp(command)) => {
            let view = match &command {
                McpCommand::Refresh(refresh) => finished(
                    &refresh.operation_id,
                    McpOperationKind::Refresh,
                    None,
                    McpOperationState::Succeeded {
                        outcome: McpOutcome::Settings {
                            revision: revision(),
                        },
                    },
                ),
                McpCommand::TestConnection(test) => {
                    let state = if test.scope.server_id == "docs" {
                        McpOperationState::Succeeded {
                            outcome: McpOutcome::Tested {
                                status: McpServerStatus::Ready,
                            },
                        }
                    } else {
                        McpOperationState::Running
                    };
                    finished(
                        &test.operation_id,
                        McpOperationKind::TestConnection,
                        Some(&test.scope.server_id),
                        state,
                    )
                }
                _ => return,
            };
            FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Mcp(
                McpResult::Operation(view),
            ))))
        }
        _ => return,
    };
    shared.features.reply(FeatureId::Mcp, tag, reply);
}

// Given: proj-1 with MCP servers that are ready, revoked, connecting, at the process
//        limit and off
// When: the MCP tab reads them, `docs` is tested and a test of `search` is running
// Then: each row shows its legacy badge and the reason, test result and work in words
#[test]
fn the_mcp_tab_shows_each_server_state_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.open_feature(FeatureId::Mcp, &mut |shared, action| {
        if !answer_artifacts(shared, &action) {
            answer_mcp(shared, action);
        }
    });
    let test_of = |screen: &mut Screen, index: usize| {
        let button = screen
            .harness
            .get_all_by_label("Test connection")
            .nth(index)
            .expect("a Test connection button");
        button.click();
        screen.serve(&mut answer_mcp);
    };
    test_of(&mut screen, 0);
    test_of(&mut screen, 2);
    for label in [
        "ready",
        "needs approval",
        "connecting",
        "process limit",
        "off",
        "Approval was revoked; calls to this server are refused.",
        "Connection test: ready.",
        "Testing the connection…",
        "Keychain entry token: a value is stored",
    ] {
        screen.harness.get_by_label(label);
    }
    assert!(
        screen
            .harness
            .query_by_label_contains("NeedsApproval")
            .is_none()
    );
    assert!(
        screen
            .harness
            .query_by_label_contains("ProcessLimitReached")
            .is_none()
    );
    assert_english(&screen);
    screen.capture("mcp");
}
