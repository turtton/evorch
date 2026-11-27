//! The Features sections of the `settings` area through the production command path:
//! one atomic save of the four sections, what follows a save at once and what waits for a
//! restart, and the models' applied state staying out of it.
mod common;

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use workbench_host::{
    CommandHost,
    areas::{
        AreaTasks, Areas,
        settings::{
            SettingsArea, SettingsParts,
            feature_options::{FeatureRuntime, Unwired},
        },
    },
    commands::CommandService,
};
use workbench_protocol::{
    commands::Command,
    errors::ErrorCode,
    ids::{OperationId, SessionId},
    namespaces::settings::*,
    operation::OperationState,
    queries::{Query, QueryReply},
};

const START: &str = "version = 3\n# keep me\n[core]\nidle_grace_secs = 7 # grace\n\n[sandbox]\n# web before\nweb_tools_enabled = true # web after\nescalation_approval = \"user\"\n\n[tools.run_js]\nenabled = true\n";

fn op(id: &str) -> OperationId {
    OperationId::new(id).unwrap()
}

/// The running core's Features as a fake the test can look at: what the settings area
/// reads from it and what it was told to apply.
struct FakeCore {
    pull_requests: Mutex<Option<config::PullRequestsConfig>>,
    web_tools: AtomicBool,
    pr_applied: Mutex<Vec<config::PullRequestsConfig>>,
    /// The PR service does not take settings.
    refuse_pull_requests: AtomicBool,
}

impl FakeCore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            pull_requests: Mutex::new(Some(config::PullRequestsConfig::default())),
            web_tools: AtomicBool::new(true),
            pr_applied: Mutex::default(),
            refuse_pull_requests: AtomicBool::new(false),
        })
    }
}

impl FeatureRuntime for FakeCore {
    fn pull_requests(&self) -> Option<config::PullRequestsConfig> {
        *self.pull_requests.lock().unwrap()
    }

    fn apply_pull_requests(&self, settings: config::PullRequestsConfig) -> Result<(), String> {
        if self.refuse_pull_requests.load(Ordering::SeqCst) {
            return Err("not started".into());
        }
        self.pr_applied.lock().unwrap().push(settings);
        *self.pull_requests.lock().unwrap() = Some(settings);
        Ok(())
    }

    fn web_tools_enabled(&self) -> Option<bool> {
        Some(self.web_tools.load(Ordering::SeqCst))
    }

    fn set_web_tools_enabled(&self, enabled: bool) {
        self.web_tools.store(enabled, Ordering::SeqCst);
    }
}

struct Fixture {
    dir: tempfile::TempDir,
    host: CommandHost,
    settings: SettingsArea,
    reloads: Arc<std::sync::atomic::AtomicUsize>,
}

impl Fixture {
    fn new(initial: &str, features: Arc<dyn FeatureRuntime>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), initial).unwrap();
        let reloads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reloads.clone();
        let service = CommandService::new(common::runtime(), tokio::runtime::Handle::current());
        let host = CommandHost::new(service);
        let source = host.projection().source("settings").unwrap();
        let settings = SettingsArea::with_parts(SettingsParts {
            config_dir: dir.path().to_path_buf(),
            // The core starts with a file that loads; the test of one that does not starts with
            // the defaults.
            startup: Arc::new(
                config::Config::load_strict(&config::LoadOptions {
                    user_config_dir: Some(dir.path().to_path_buf()),
                    read_env: false,
                    ..Default::default()
                })
                .unwrap_or_default(),
            ),
            reload_models: Arc::new(move || {
                counted.fetch_add(1, Ordering::SeqCst);
                Ok(())
            }),
            tasks: AreaTasks::new(tokio::runtime::Handle::current()),
            source,
            features,
        });
        let host = host.with_areas(Areas {
            settings: settings.clone(),
            ..Default::default()
        });
        Self {
            dir,
            host,
            settings,
            reloads,
        }
    }

    fn file(&self) -> String {
        std::fs::read_to_string(self.dir.path().join("config.toml")).unwrap()
    }

    fn query(&self, query: SettingsQuery) -> SettingsReply {
        match self
            .host
            .handle_query(
                &SessionId::new("session").unwrap(),
                &Query::Settings(query),
                &workbench_host::provenance::local_desktop(),
            )
            .unwrap()
        {
            QueryReply::Settings(reply) => reply,
            other => panic!("{other:?}"),
        }
    }

    fn view(&self) -> FeatureSettingsView {
        match self.query(SettingsQuery::GetFeatureSettings {}) {
            SettingsReply::FeatureSettings(view) => *view,
            other => panic!("{other:?}"),
        }
    }

    fn snapshot(&self) -> SettingsSnapshot {
        match self.query(SettingsQuery::GetSettingsSnapshot {}) {
            SettingsReply::Settings(snapshot) => *snapshot,
            other => panic!("{other:?}"),
        }
    }

    async fn run(&mut self, id: &str, command: SettingsCommand) -> SettingsOperationView {
        self.host
            .handle_command(Command::Settings(command), common::web())
            .unwrap();
        self.settings.wait_operation(&op(id)).await.unwrap()
    }

    async fn save_features(
        &mut self,
        id: &str,
        expected: SettingsRevision,
        changes: FeatureSettingsPatch,
    ) -> SettingsOperationView {
        self.run(
            id,
            SettingsCommand::SaveFeatureSettings(SaveFeatureSettings {
                operation_id: op(id),
                expected_revision: expected,
                changes,
            }),
        )
        .await
    }

    async fn reload(&mut self, id: &str) {
        let view = self
            .run(
                id,
                SettingsCommand::ReloadModels(ReloadModels {
                    operation_id: op(id),
                }),
            )
            .await;
        assert_eq!(view.operation.state, OperationState::Ready);
    }
}

fn pr(enabled: bool) -> PullRequestFeature {
    PullRequestFeature {
        enabled,
        auto_fix_enabled: true,
        include_in_review: true,
        pending_poll_seconds: 30,
        stable_poll_seconds: 300,
        timeout_seconds: 20,
    }
}

fn browser(max_tabs: u32) -> BrowserFeature {
    BrowserFeature {
        enabled: true,
        executable: Some("/opt/chromium".into()),
        max_pages: 8,
        max_tabs,
        coordinate_actions: true,
    }
}

fn cleanup(archive: u32) -> ThreadCleanupFeature {
    ThreadCleanupFeature {
        auto_archive_after_days: archive,
        archived_delete_after_days: 7,
    }
}

fn all_sections() -> FeatureSettingsPatch {
    FeatureSettingsPatch {
        pull_requests: Some(pr(true)),
        agent_web_tools: Some(false),
        browser: Some(browser(2)),
        thread_cleanup: Some(cleanup(0)),
    }
}

#[tokio::test]
async fn four_sections_are_saved_in_one_write_and_the_other_keys_stay() {
    // Given: a file with unrelated keys and comments, and a running core.
    let core = FakeCore::new();
    let mut f = Fixture::new(START, core.clone());
    let before = f.view();
    assert!(before.restart_pending.is_empty());

    // When: all four sections are saved at once.
    let done = f
        .save_features("save-all", before.revision.clone(), all_sections())
        .await;

    // Then: the file has every value, and every other key and comment is as it was.
    assert_eq!(done.operation.state, OperationState::Ready);
    assert_eq!(done.message, None);
    let file = f.file();
    let loaded = config::Config::load_strict(&config::LoadOptions {
        user_config_dir: Some(f.dir.path().to_path_buf()),
        read_env: false,
        ..Default::default()
    })
    .unwrap();
    assert!(loaded.pull_requests.enabled);
    assert!(!loaded.sandbox.web_tools_enabled);
    assert_eq!(loaded.tools.browser.max_tabs.get(), 2);
    assert_eq!(loaded.storage.thread_retention.auto_archive_after_days, 0);
    assert!(file.contains("# keep me\n"), "{file}");
    assert!(file.contains("idle_grace_secs = 7 # grace\n"), "{file}");
    assert!(file.contains("escalation_approval = \"user\""), "{file}");
    assert!(file.contains("[tools.run_js]\nenabled = true"), "{file}");
    assert_eq!(done.revision.as_ref(), Some(&f.view().revision));
}

#[tokio::test]
async fn the_pr_view_and_web_tools_follow_a_save_at_once_and_the_rest_waits_for_a_restart() {
    let core = FakeCore::new();
    let mut f = Fixture::new(START, core.clone());
    let revision = f.view().revision;

    // Nothing reaches the running core before a save.
    assert!(core.pr_applied.lock().unwrap().is_empty());
    assert!(core.web_tools.load(Ordering::SeqCst));

    f.save_features("save-all", revision, all_sections()).await;

    // The PR view and Agent web tools are applied from what was read back.
    assert_eq!(core.pr_applied.lock().unwrap().len(), 1);
    assert!(core.pr_applied.lock().unwrap()[0].enabled);
    assert!(!core.web_tools.load(Ordering::SeqCst));
    let view = f.view();
    assert!(view.running.pull_requests.enabled && view.saved.pull_requests.enabled);
    assert!(!view.running.agent_web_tools && !view.saved.agent_web_tools);
    // The browser and Thread cleanup are what the core started with; saved differs.
    assert_eq!(
        view.restart_pending,
        [FeatureSection::Browser, FeatureSection::ThreadCleanup]
    );
    assert_eq!(view.running.browser.max_tabs, 4);
    assert_eq!(view.saved.browser.max_tabs, 2);
    assert_eq!(view.running.thread_cleanup.auto_archive_after_days, 7);
    assert_eq!(view.saved.thread_cleanup.auto_archive_after_days, 0);
}

#[tokio::test]
async fn a_section_the_save_does_not_name_is_not_applied() {
    let core = FakeCore::new();
    let mut f = Fixture::new(START, core.clone());
    let revision = f.view().revision;

    f.save_features(
        "web-only",
        revision,
        FeatureSettingsPatch {
            agent_web_tools: Some(false),
            ..Default::default()
        },
    )
    .await;

    assert!(!core.web_tools.load(Ordering::SeqCst));
    assert!(core.pr_applied.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_refused_save_changes_neither_the_file_nor_the_running_core() {
    let core = FakeCore::new();
    let mut f = Fixture::new(START, core.clone());
    let revision = f.view().revision;
    let stale = SettingsRevision::from_bytes(&[7; 32]);

    // A stale revision.
    let stale_save = f.save_features("stale", stale, all_sections()).await;
    assert_eq!(
        stale_save.operation.state,
        OperationState::Failed {
            code: ErrorCode::StaleRevision
        }
    );
    // A value the configuration does not accept: stable polling shorter than pending.
    let invalid = f
        .save_features(
            "invalid",
            revision.clone(),
            FeatureSettingsPatch {
                pull_requests: Some(PullRequestFeature {
                    pending_poll_seconds: 600,
                    stable_poll_seconds: 60,
                    ..pr(true)
                }),
                agent_web_tools: Some(false),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(
        invalid.operation.state,
        OperationState::Failed {
            code: ErrorCode::InvalidRequest
        }
    );

    assert_eq!(f.file(), START);
    assert!(core.pr_applied.lock().unwrap().is_empty());
    assert!(core.web_tools.load(Ordering::SeqCst));
    assert_eq!(f.view().revision, revision);
}

#[tokio::test]
async fn a_pending_restart_goes_away_when_the_value_is_put_back() {
    let mut f = Fixture::new(START, FakeCore::new());
    let revision = f.view().revision;

    f.save_features(
        "raise",
        revision,
        FeatureSettingsPatch {
            browser: Some(browser(2)),
            ..Default::default()
        },
    )
    .await;
    let waiting = f.view();
    assert_eq!(waiting.restart_pending, [FeatureSection::Browser]);

    // The startup values are the defaults: saving them again leaves nothing waiting.
    let back = f
        .save_features(
            "back",
            waiting.revision,
            FeatureSettingsPatch {
                browser: Some(BrowserFeature {
                    enabled: true,
                    executable: None,
                    max_pages: 8,
                    max_tabs: 4,
                    coordinate_actions: false,
                }),
                ..Default::default()
            },
        )
        .await;
    assert_eq!(back.operation.state, OperationState::Ready);
    assert!(f.view().restart_pending.is_empty());
    assert!(!f.file().contains("executable"), "{}", f.file());
}

#[tokio::test]
async fn reloading_models_neither_clears_nor_creates_a_restart_wait() {
    let mut f = Fixture::new(START, FakeCore::new());
    let revision = f.view().revision;
    f.save_features(
        "cleanup",
        revision,
        FeatureSettingsPatch {
            thread_cleanup: Some(cleanup(14)),
            ..Default::default()
        },
    )
    .await;
    assert_eq!(f.view().restart_pending, [FeatureSection::ThreadCleanup]);

    f.reload("reload").await;

    assert_eq!(f.reloads.load(Ordering::SeqCst), 1);
    assert_eq!(f.view().restart_pending, [FeatureSection::ThreadCleanup]);
}

#[tokio::test]
async fn a_features_save_does_not_make_the_models_wait_for_a_reload() {
    let mut f = Fixture::new(START, FakeCore::new());
    let revision = f.view().revision;
    assert!(!f.snapshot().pending_apply());

    f.save_features("features", revision, all_sections()).await;

    let snapshot = f.snapshot();
    assert!(
        !snapshot.pending_apply(),
        "a save of Features keys leaves nothing for Reload models"
    );
    assert_eq!(snapshot.applied_revision.as_ref(), Some(&snapshot.revision));
    assert_eq!(f.reloads.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_reload_that_was_already_waiting_keeps_waiting_after_a_features_save() {
    let mut f = Fixture::new(START, FakeCore::new());
    // A whole-text save that the models have not been reloaded for.
    let revision = f.snapshot().revision;
    let done = f
        .run(
            "document",
            SettingsCommand::SaveDocument(SaveDocument {
                operation_id: op("document"),
                expected_revision: revision,
                text: format!("{START}# edited\n"),
            }),
        )
        .await;
    assert_eq!(done.operation.state, OperationState::Ready);
    assert!(f.snapshot().pending_apply());

    let revision = f.view().revision;
    f.save_features(
        "features",
        revision,
        FeatureSettingsPatch {
            agent_web_tools: Some(false),
            ..Default::default()
        },
    )
    .await;

    assert!(f.snapshot().pending_apply());
    f.reload("reload").await;
    assert!(!f.snapshot().pending_apply());
}

#[tokio::test]
async fn a_failure_to_apply_is_a_saved_file_and_a_restart_wait_not_a_failed_save() {
    let core = FakeCore::new();
    core.refuse_pull_requests.store(true, Ordering::SeqCst);
    let mut f = Fixture::new(START, core);
    let revision = f.view().revision;

    let done = f
        .save_features(
            "pr",
            revision,
            FeatureSettingsPatch {
                pull_requests: Some(pr(true)),
                ..Default::default()
            },
        )
        .await;

    assert_eq!(done.operation.state, OperationState::Ready);
    assert!(done.message.as_deref().unwrap().contains("not applied"));
    let view = f.view();
    assert!(view.saved.pull_requests.enabled && !view.running.pull_requests.enabled);
    assert_eq!(view.restart_pending, [FeatureSection::PullRequests]);
}

#[tokio::test]
async fn a_value_another_layer_overrides_is_refused() {
    let mut f = Fixture::new(START, FakeCore::new());
    let drop_ins = f.dir.path().join("config.d");
    std::fs::create_dir_all(&drop_ins).unwrap();
    std::fs::write(
        drop_ins.join("10-web.toml"),
        "version = 3\n[sandbox]\nweb_tools_enabled = true\n",
    )
    .unwrap();
    let revision = f.view().revision;

    let done = f
        .save_features(
            "override",
            revision,
            FeatureSettingsPatch {
                agent_web_tools: Some(false),
                ..Default::default()
            },
        )
        .await;

    assert_eq!(
        done.operation.state,
        OperationState::Failed {
            code: ErrorCode::InvalidRequest
        }
    );
    assert!(done.message.unwrap().contains("overrides"));
    assert_eq!(f.file(), START);
}

#[tokio::test]
async fn restart_targets_are_kept_by_a_features_save() {
    let text = format!("{START}[core.restart.targets.web]\npath = '/usr/bin/true'\n");
    let mut f = Fixture::new(&text, FakeCore::new());
    let revision = f.view().revision;

    let done = f.save_features("keep", revision, all_sections()).await;

    assert_eq!(done.operation.state, OperationState::Ready);
    assert!(
        f.file()
            .contains("[core.restart.targets.web]\npath = '/usr/bin/true'\n"),
        "{}",
        f.file()
    );
}

#[tokio::test]
async fn an_empty_patch_is_refused_before_it_reaches_the_file() {
    let mut f = Fixture::new(START, FakeCore::new());
    let revision = f.view().revision;
    let refused = f
        .host
        .handle_command(
            Command::Settings(SettingsCommand::SaveFeatureSettings(SaveFeatureSettings {
                operation_id: op("empty"),
                expected_revision: revision,
                changes: FeatureSettingsPatch::default(),
            })),
            common::web(),
        )
        .unwrap_err();
    assert_eq!(refused.code, ErrorCode::InvalidRequest);
    assert_eq!(f.file(), START);
}

#[tokio::test]
async fn a_file_that_does_not_load_is_a_problem_of_the_view() {
    let f = Fixture::new("version = 3\n[core\n", Arc::new(Unwired));
    let view = f.view();
    assert_eq!(view.problem, Some(SettingsProblem::InvalidConfig));
    assert_eq!(view.saved, view.running);
    assert!(view.restart_pending.is_empty());
}

#[tokio::test]
async fn the_agent_runtime_follows_the_web_tools_switch_and_says_when_it_has_no_pr_service() {
    let runtime = common::runtime();
    let mut f = Fixture::new(START, Arc::new(runtime.clone()));
    let revision = f.view().revision;
    assert!(runtime.web_tools_enabled());

    let done = f
        .save_features(
            "runtime",
            revision,
            FeatureSettingsPatch {
                pull_requests: Some(pr(true)),
                agent_web_tools: Some(false),
                ..Default::default()
            },
        )
        .await;

    assert!(!runtime.web_tools_enabled());
    assert_eq!(done.operation.state, OperationState::Ready);
    assert!(done.message.unwrap().contains("not applied"));
    assert!(!f.view().running.agent_web_tools);
    assert_eq!(f.view().restart_pending, [FeatureSection::PullRequests]);
}
