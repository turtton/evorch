// Headless deterministic --demo goal loop test (issue #73 T3.2 / S10).
// The real runtime drives DemoScriptModel keyed scripts through the
// production RuntimeCommandSink: submit DEMO-GOAL, observe the full
// orchestrator loop (implement -> review request-update -> repair -> approve
// -> continuation finish -> merge approval), then approve the merge and reach
// `state: complete`. The whole flow is executed twice and must produce an
// identical OrchestratorEvent sequence modulo goal/run/token ids.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use event_bus::{EventBus, EventKind, GoalState, OrchestratorEvent, RecvError};
use gui::app::WorkbenchState;
use gui::events::EventPump;
use gui::headless::HeadlessWorkbench;
use gui::model::demo::DemoScriptModel;
use gui::runtime_sink::RuntimeCommandSink;
use runtime::workspace::{Project, WorktreeManager};
use runtime::{
    AgentRuntime, ExecutionPolicy, FixtureDeliveryAdapter, GoalSupervisor, IsolatedMounts,
    OrchestrationSettings, SandboxFactory,
};
use sandbox::{DirectSandbox, Sandbox, SandboxError};
use tools::ToolExecutor;
use workspace_ui::{PanelId, ProjectId, SidebarState, ThreadId, UiSettings};

const DEMO_GOAL: &str = "DEMO-GOAL implement fixture unit";
const LABEL_TIMEOUT: Duration = Duration::from_secs(20);

/// isolated workspace 用に mounts を記録しつつ DirectSandbox を返す factory。
struct RecordingSandboxFactory {
    #[expect(dead_code)]
    mounts: Arc<Mutex<Vec<IsolatedMounts>>>,
}

impl SandboxFactory for RecordingSandboxFactory {
    fn build(
        &self,
        _policy: &ExecutionPolicy,
        mounts: &IsolatedMounts,
    ) -> Result<Arc<dyn Sandbox>, SandboxError> {
        self.mounts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(mounts.clone());
        Ok(Arc::new(DirectSandbox::new_unchecked()))
    }
}

/// --demo 相当の 1 commit git リポジトリを一時ディレクトリに作成する。
fn init_demo_repo() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().expect("temp dir");
    let repo = temp.path().join("repo");
    std::fs::create_dir(&repo).expect("repo directory");
    for args in [
        vec!["init", "--quiet"],
        vec!["config", "user.email", "demo@evorch.local"],
        vec!["config", "user.name", "evorch demo"],
        vec![
            "commit",
            "--allow-empty",
            "--quiet",
            "-m",
            "initial demo commit",
        ],
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(&repo)
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }
    (temp, repo)
}

fn sidebar_with_thread(root: &Path) -> SidebarState {
    let mut sidebar = SidebarState::default();
    let project_id = ProjectId::new("demo");
    sidebar
        .add_project(project_id.clone(), "demo", root)
        .expect("project can be added");
    sidebar
        .select_project(&project_id)
        .expect("project can be selected");
    sidebar
        .create_thread(ThreadId::new("thread-1"), project_id, "thread-1")
        .expect("thread can be created");
    sidebar
        .switch_thread(&ThreadId::new("thread-1"))
        .expect("thread can be selected");
    sidebar
}

fn activate_panel(harness: &mut HeadlessWorkbench<AgentRuntime>, panel_id: &str) {
    let dock = harness.state_mut().dock_mut();
    let path = dock
        .find_tab(&PanelId::new(panel_id))
        .expect("panel tab exists");
    let leaf = dock.leaf_mut(path.node_path()).expect("leaf exists");
    leaf.set_active_tab(path.tab.0).expect("tab index is valid");
}

/// bus 上の OrchestratorEvent を発行順に収集する。
fn spawn_collector(
    runtime: &tokio::runtime::Runtime,
    bus: &Arc<EventBus>,
) -> Arc<Mutex<Vec<OrchestratorEvent>>> {
    let collected: Arc<Mutex<Vec<OrchestratorEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&collected);
    let mut receiver = bus.subscribe();
    runtime.spawn(async move {
        loop {
            match receiver.recv().await {
                Ok(event) => {
                    if let EventKind::Orchestrator(orchestrator) = event.kind {
                        lock(&sink).push(orchestrator);
                    }
                }
                Err(RecvError::Lagged(skipped)) => {
                    panic!("demo event collector lagged by {skipped} events");
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
    collected
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// --demo と同じ構成 (DemoScriptModel + FixtureDeliveryAdapter::scripted_happy_path)
/// の headless workbench fixture。
struct DemoFixture {
    runtime: tokio::runtime::Runtime,
    _temp: tempfile::TempDir,
    repaint_rx: mpsc::Receiver<()>,
    harness: HeadlessWorkbench<AgentRuntime>,
    collected: Arc<Mutex<Vec<OrchestratorEvent>>>,
}

impl DemoFixture {
    fn new() -> Self {
        let rt = tokio::runtime::Runtime::new().expect("multi-thread test runtime");
        let (temp, repo) = init_demo_repo();
        let bus = Arc::new(EventBus::new(1024));
        let executor = Arc::new(ToolExecutor::with_standard_tools(
            Arc::clone(&bus),
            Arc::new(DirectSandbox::new_unchecked()),
        ));
        let manager = WorktreeManager::new(Project::new(repo.clone()).expect("git repo"));
        let factory: Arc<dyn SandboxFactory> = Arc::new(RecordingSandboxFactory {
            mounts: Arc::new(Mutex::new(Vec::new())),
        });
        let model = Arc::new(DemoScriptModel::new(Arc::clone(&bus)).with_workspace_root(&repo));
        let runtime = AgentRuntime::with_workspace_context(
            Arc::clone(&bus),
            executor,
            model,
            manager,
            factory,
        );
        let supervisor = rt.block_on(async {
            GoalSupervisor::spawn(
                runtime.clone(),
                Arc::clone(&bus),
                Arc::new(FixtureDeliveryAdapter::scripted_happy_path()),
                OrchestrationSettings::default(),
            )
        });
        let (repaint_tx, repaint_rx) = mpsc::channel();
        let pump = EventPump::spawn(
            rt.handle(),
            bus.subscribe(),
            Some(Arc::new(move || {
                let _ = repaint_tx.send(());
            })),
        );
        let collected = spawn_collector(&rt, &bus);
        let state = WorkbenchState::new(runtime.clone(), &UiSettings::default())
            .expect("default state builds")
            .with_pump(pump)
            .with_sidebar(sidebar_with_thread(&repo))
            .with_command_sink(Box::new(RuntimeCommandSink::new(
                runtime.clone(),
                rt.handle().clone(),
                supervisor,
            )));
        let mut harness = HeadlessWorkbench::new(state, [1200.0, 800.0]);
        activate_panel(&mut harness, "goal-main");
        harness.run();
        Self {
            runtime: rt,
            _temp: temp,
            repaint_rx,
            harness,
            collected,
        }
    }

    fn run_until(&mut self, label: &str) {
        let deadline = Instant::now() + LABEL_TIMEOUT;
        while !self.harness.has_label(label) {
            assert!(
                Instant::now() < deadline,
                "label {label:?} did not appear within {LABEL_TIMEOUT:?}; events: {:#?}",
                lock(&self.collected)
            );
            let _ = self.repaint_rx.recv_timeout(Duration::from_millis(200));
            self.harness.run();
        }
    }

    /// DEMO-GOAL を投入し、merge approve まで駆動して goal を complete させる。
    fn drive_demo_goal(&mut self) {
        self.harness.state_mut().goal_form_mut().goal = DEMO_GOAL.into();
        self.harness.run();
        self.harness.click_label("Submit");
        self.harness.run();
        self.run_until("accepted: goal-1");

        // 配信パイプラインが PR を出す前の早期 finish は gate に拒否され、
        // run は finish せずに終わる (continuation epoch が後で発火する)。
        self.run_until("rejected: no_pull_request");
        self.run_until("stage: awaiting_merge_approval");

        activate_panel(&mut self.harness, "merge-main");
        // FixtureDeliveryAdapter::scripted_happy_path の PR #101 head a2…。
        self.run_until("head: a2a2a2a2");
        assert!(self.harness.has_label("gate: pull_request ok"));
        assert!(self.harness.has_label("gate: ci ok"));
        self.harness.click_label("Approve");
        self.harness.run();

        activate_panel(&mut self.harness, "goal-main");
        self.run_until("state: complete");
        assert!(self.harness.has_label("closeout: worker_claim ok"));
        assert!(self.harness.has_label("closeout: result_summary ok"));
        assert!(self.harness.has_label("closeout: worker_complete ok"));

        // collector が末尾イベント (Complete → Done stage) まで受信するのを待つ。
        let deadline = Instant::now() + LABEL_TIMEOUT;
        loop {
            let done = lock(&self.collected).iter().any(|event| {
                matches!(
                    event,
                    OrchestratorEvent::GoalStateChanged {
                        to: GoalState::Complete,
                        ..
                    }
                )
            });
            if done {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "collector did not observe goal completion within {LABEL_TIMEOUT:?}"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    fn events(&self) -> Vec<OrchestratorEvent> {
        lock(&self.collected).clone()
    }
}

/// 比較用に goal/run/token の動的 ID を出現順のプレースホルダへ正規化する。
///
/// 隣接する完全同一イベントは畳み込む。runtime は worker 起動時に
/// `attach_goal_child` (delegate tool 経路) と supervisor の
/// `on_run_started` の双方から同一の RunAttached を emit しうる (ledger
/// apply は冪等なので状態には影響しないが、bus 上のイベント列には
/// スケジューリング次第で重複が現れる)。runtime API は frozen のため、
/// demo 側の決定性はこの既知の重複を除いた列で評価する。
fn normalized_sequence(events: &[OrchestratorEvent]) -> Vec<serde_json::Value> {
    let mut goals = HashMap::new();
    let mut runs = HashMap::new();
    let mut tokens = HashMap::new();
    let mut normalized: Vec<serde_json::Value> = Vec::new();
    for event in events {
        let mut value = serde_json::to_value(event).expect("orchestrator event serializes");
        normalize_value(&mut value, &mut goals, &mut runs, &mut tokens);
        if normalized.last() == Some(&value) {
            continue;
        }
        normalized.push(value);
    }
    normalized
}

fn normalize_value(
    value: &mut serde_json::Value,
    goals: &mut HashMap<String, String>,
    runs: &mut HashMap<String, String>,
    tokens: &mut HashMap<String, String>,
) {
    match value {
        serde_json::Value::String(text) => *text = normalize_ids(text, goals, runs, tokens),
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_value(item, goals, runs, tokens);
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                normalize_value(item, goals, runs, tokens);
            }
        }
        _ => {}
    }
}

fn normalize_ids(
    text: &str,
    goals: &mut HashMap<String, String>,
    runs: &mut HashMap<String, String>,
    tokens: &mut HashMap<String, String>,
) -> String {
    let mut output = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find(|character: char| character.is_ascii_alphanumeric()) {
        output.push_str(&rest[..start]);
        let tail = &rest[start..];
        let end = tail
            .find(|character: char| {
                !(character.is_ascii_alphanumeric() || character == '-' || character == '_')
            })
            .unwrap_or(tail.len());
        let (word, remainder) = tail.split_at(end);
        output.push_str(&normalize_word(word, goals, runs, tokens));
        rest = remainder;
    }
    output.push_str(rest);
    output
}

fn normalize_word(
    word: &str,
    goals: &mut HashMap<String, String>,
    runs: &mut HashMap<String, String>,
    tokens: &mut HashMap<String, String>,
) -> String {
    let placeholder =
        if word.starts_with("goal-") && word[5..].chars().all(|c| c.is_ascii_digit() || c == '-') {
            Some(("goal", goals))
        } else if word.starts_with("run-") && word[4..].chars().all(|c| c.is_ascii_digit()) {
            Some(("run", runs))
        } else if word.len() == 32 && word.chars().all(|c| c.is_ascii_hexdigit()) {
            Some(("token", tokens))
        } else {
            None
        };
    let Some((prefix, map)) = placeholder else {
        return word.to_string();
    };
    let next = map.len();
    map.entry(word.to_string())
        .or_insert_with(|| format!("{prefix}-{}", (next as u8 + b'A') as char))
        .clone()
}

/// ドキュメント化された demo loop のマイルストーンが順序どおりに現れることを
/// 検証する。
fn assert_milestone_order(events: &[OrchestratorEvent]) {
    let milestones: Vec<fn(&OrchestratorEvent) -> bool> = vec![
        |event| matches!(event, OrchestratorEvent::GoalCreated { .. }),
        |event| {
            matches!(
                event,
                OrchestratorEvent::RunAttached {
                    purpose: event_bus::RunPurpose::Implement,
                    ..
                }
            )
        },
        |event| matches!(event, OrchestratorEvent::FinishRejected { .. }),
        |event| matches!(event, OrchestratorEvent::DeliverableBranchBound { .. }),
        |event| {
            matches!(
                event,
                OrchestratorEvent::ReviewRoundStarted { round: 1, .. }
            )
        },
        |event| matches!(event, OrchestratorEvent::RepairDispatched { round: 1, .. }),
        |event| {
            matches!(
                event,
                OrchestratorEvent::ReviewRoundStarted { round: 2, .. }
            )
        },
        |event| {
            matches!(
                event,
                OrchestratorEvent::GoalStageChanged {
                    to: event_bus::GoalStage::ReadyToFinish,
                    ..
                }
            )
        },
        |event| {
            matches!(
                event,
                OrchestratorEvent::ContinuationDispatched { epoch: 1, .. }
            )
        },
        |event| matches!(event, OrchestratorEvent::FinishAccepted { .. }),
        |event| matches!(event, OrchestratorEvent::MergeApprovalRequested { .. }),
        |event| {
            matches!(
                event,
                OrchestratorEvent::MergeApprovalResolved {
                    decision: event_bus::ApprovalDecision::Approved,
                    ..
                }
            )
        },
        |event| matches!(event, OrchestratorEvent::MergeExecuted { ok: true, .. }),
        |event| {
            matches!(
                event,
                OrchestratorEvent::GoalStateChanged {
                    to: GoalState::Complete,
                    ..
                }
            )
        },
    ];
    let mut positions = Vec::new();
    let mut search_from = 0;
    for milestone in &milestones {
        let position = events[search_from..]
            .iter()
            .position(milestone)
            .map(|index| index + search_from)
            .unwrap_or_else(|| panic!("milestone missing from demo event sequence: {events:#?}"));
        positions.push(position);
        search_from = position + 1;
    }
    assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));

    let rejection = events
        .iter()
        .find_map(|event| match event {
            OrchestratorEvent::FinishRejected { rejections, .. } => Some(rejections),
            _ => None,
        })
        .expect("demo loop records an early finish rejection");
    assert!(
        rejection
            .iter()
            .any(|item| matches!(item, event_bus::GateRejection::NoPullRequest)),
        "early finish must be rejected with no_pull_request: {rejection:?}"
    );
}

#[test]
fn demo_goal_reaches_awaiting_merge_then_complete_deterministically() {
    // Given/When/Then: 1 回目の demo loop を完走させる。
    let mut first = DemoFixture::new();
    first.drive_demo_goal();
    let first_events = first.events();
    assert_milestone_order(&first_events);

    // When: 同一構成の fixture で 2 回目を完走させる。
    let mut second = DemoFixture::new();
    second.drive_demo_goal();
    let second_events = second.events();

    // Then: goal/run/token ID とタイミングを除き、イベント列が完全一致する。
    let normalized_first = normalized_sequence(&first_events);
    let normalized_second = normalized_sequence(&second_events);
    assert_eq!(
        normalized_first, normalized_second,
        "demo loop event sequence must be deterministic"
    );
    drop(first);
}
