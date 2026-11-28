//! The footer as the embedded workbench draws it: the cost of today on the usage button,
//! the theme picker under ⚙, and the status line in English.
use super::*;
use crate::core_client::features::diagnostics::DiagnosticsFeature;
use crate::panes::system_footer::{
    SystemFooter,
    stats::{SystemSnapshot, Usage},
};
use crate::panes::theme_widgets::NO_SETTINGS_PATH;
use crate::theme::style::{ThemePreset, style_for};
use std::time::Duration;
use workbench_protocol::{
    ids::OperationId,
    namespaces::diagnostics::{
        ContextMessage, ContextMessageRole, ContextSegment, DiagnosticsQuery, DiagnosticsReply,
        GetRunContext, ReadOutcome, RunContext, RunContextOverview, UsageDay, UsageModel,
        UsageRange, UsageSummary, UsageTotals,
    },
    operation::{OperationState, OperationView},
};

fn todays_summary(actions: Vec<Action>) -> (u64, OperationView) {
    actions
        .into_iter()
        .find_map(|action| match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Diagnostics,
                tag,
                request:
                    FeatureRequest::Query(Query::Diagnostics(DiagnosticsQuery::GetUsageSummary(get))),
            }) if get.range == UsageRange::Today => Some((
                tag,
                OperationView {
                    operation_id: get.operation_id,
                    state: OperationState::Ready,
                    revision: Revision(1),
                    result: None,
                },
            )),
            _ => None,
        })
        .expect("today's usage was asked")
}

fn today(operation: OperationView, cost_micro_usd: u64) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
        Box::new(ReadOutcome {
            operation,
            data: Some(UsageSummary {
                range: UsageRange::Today,
                from_day: "2026-11-25".into(),
                to_day: "2026-11-25".into(),
                totals: UsageTotals {
                    requests: 3,
                    recorded_cost_micro_usd: cost_micro_usd,
                    ..Default::default()
                },
                days: vec![],
                days_omitted: 0,
                models: vec![],
                models_omitted: 0,
            }),
        }),
    ))))
}

fn ready(operation_id: OperationId) -> OperationView {
    OperationView {
        operation_id,
        state: OperationState::Ready,
        revision: Revision(1),
        result: None,
    }
}

fn diagnostics_asked(screen: &mut Screen) -> Vec<(u64, DiagnosticsQuery)> {
    screen.harness.run_steps(2);
    let mut asked = Vec::new();
    while let Ok(action) = screen.actions.try_recv() {
        if let Action::Feature(FeatureAction {
            feature: FeatureId::Diagnostics,
            tag,
            request: FeatureRequest::Query(Query::Diagnostics(query)),
        }) = action
        {
            asked.push((tag, query));
        }
    }
    asked
}

fn last_week(operation: OperationView) -> FeatureReply {
    let day = |day: &str, requests: u64, micro_usd: u64| UsageDay {
        day: day.into(),
        totals: UsageTotals {
            requests,
            input_tokens: 40_000 * requests,
            output_tokens: 3_000 * requests,
            cache_read_tokens: 25_000 * requests,
            recorded_cost_micro_usd: micro_usd,
            ..Default::default()
        },
    };
    let days = vec![
        day("2026-11-19", 4, 420_000),
        day("2026-11-21", 9, 1_100_000),
        day("2026-11-24", 6, 730_000),
    ];
    let mut totals = UsageTotals::default();
    for listed in &days {
        totals.requests += listed.totals.requests;
        totals.input_tokens += listed.totals.input_tokens;
        totals.output_tokens += listed.totals.output_tokens;
        totals.cache_read_tokens += listed.totals.cache_read_tokens;
        totals.recorded_cost_micro_usd += listed.totals.recorded_cost_micro_usd;
    }
    let models = vec![
        UsageModel {
            provider: "anthropic".into(),
            model: "claude-opus".into(),
            totals: UsageTotals {
                recorded_cost_micro_usd: 1_800_000,
                ..totals.clone()
            },
        },
        UsageModel {
            provider: "openai".into(),
            model: "gpt".into(),
            totals: UsageTotals {
                requests: 3,
                recorded_cost_micro_usd: 450_000,
                ..Default::default()
            },
        },
    ];
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Usage(
        Box::new(ReadOutcome {
            operation,
            data: Some(UsageSummary {
                range: UsageRange::Last7Days,
                from_day: "2026-11-19".into(),
                to_day: "2026-11-25".into(),
                totals,
                days,
                days_omitted: 0,
                models,
                models_omitted: 0,
            }),
        }),
    ))))
}

fn run_context(get: &GetRunContext) -> FeatureReply {
    let message = |index: u32, role, summary: &str, tokens| ContextMessage {
        index,
        role,
        summary: summary.into(),
        tokens,
        listed: true,
    };
    FeatureReply::Query(Ok(QueryReply::Diagnostics(DiagnosticsReply::Context(
        Box::new(ReadOutcome {
            operation: ready(get.operation_id.clone()),
            data: Some(RunContext {
                overview: RunContextOverview {
                    run: get.run.clone(),
                    role: "orchestrator".into(),
                    category: None,
                    phase: "after_tool_round".into(),
                    selected_model: Some("claude-opus".into()),
                    updated_at_ms: 0,
                    tools: vec!["read_file".into(), "bash".into()],
                    tools_omitted: 0,
                    message_count: 2,
                    compaction_checkpoints: 0,
                    segments: vec![
                        ContextSegment {
                            label: "system".into(),
                            tokens: 6_200,
                        },
                        ContextSegment {
                            label: "user".into(),
                            tokens: 340,
                        },
                    ],
                },
                messages: Page {
                    items: vec![
                        message(
                            0,
                            ContextMessageRole::System,
                            "You are the orchestrator",
                            6_200,
                        ),
                        message(1, ContextMessageRole::User, "Run the auth tests", 340),
                    ],
                    next_cursor: None,
                    snapshot_revision: Revision(1),
                },
            }),
        }),
    ))))
}

// Given: a connected window whose usage tab is not open
// When: the core answers today's usage, the button is pressed and the last 7 days are answered
// Then: the button shows the cost of today, and the Usage tab shows the legacy overview and a run's context
#[test]
fn the_usage_button_shows_the_cost_of_today_and_opens_the_usage_tab() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let (tag, operation) = todays_summary(screen.settle());
    screen.harness.get_by_label("Usage");

    screen.shared.lock().unwrap().features.reply(
        FeatureId::Diagnostics,
        tag,
        today(operation, 1_500_000),
    );
    screen.harness.run_steps(2);
    assert!(screen.harness.query_by_label("Usage").is_none());
    screen.harness.get_by_label("$1.50 today").click();
    let asked = diagnostics_asked(&mut screen);
    assert!(
        screen
            .harness
            .state()
            .dock
            .find_tab(&CoreTab::Feature(FeatureId::Diagnostics))
            .is_some()
    );
    let (tag, get) = asked
        .into_iter()
        .find_map(|(tag, query)| match query {
            DiagnosticsQuery::GetUsageSummary(get) if get.range == UsageRange::Last7Days => {
                Some((tag, get))
            }
            _ => None,
        })
        .expect("the last 7 days were asked");
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Diagnostics,
        tag,
        last_week(ready(get.operation_id)),
    );
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Usage tab");
    screen.harness.get_by_label("Usage");
    for label in [
        "2026-11-19 to 2026-11-25",
        "$2.25",
        "Daily tokens",
        "claude-opus",
    ] {
        screen.harness.get_by_label(label);
    }
    screen.capture("usage");

    screen
        .harness
        .get_by_role_and_label(egui::accesskit::Role::Button, "Context")
        .click();
    let (tag, get) = diagnostics_asked(&mut screen)
        .into_iter()
        .find_map(|(tag, query)| match query {
            DiagnosticsQuery::GetRunContext(get) => Some((tag, get)),
            _ => None,
        })
        .expect("the context of the thread's run was asked");
    assert_eq!(get.run.as_str(), "run-1");
    screen
        .shared
        .lock()
        .unwrap()
        .features
        .reply(FeatureId::Diagnostics, tag, run_context(&get));
    screen.harness.run_steps(2);
    screen
        .harness
        .get_by_label("user · 340 · Run the auth tests");
    screen.capture("usage-context");
}

// Given: a cost of today is shown / When: the connection is lost / Then: the button reads Usage again
#[test]
fn a_lost_connection_hides_the_cost_of_today() {
    let mut screen = screen(egui::vec2(1280.0, 800.0));
    let (tag, operation) = todays_summary(screen.settle());
    {
        let mut shared = screen.shared.lock().unwrap();
        shared
            .features
            .reply(FeatureId::Diagnostics, tag, today(operation, 1_500_000));
        shared.disconnect();
    }
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Usage");
}

// Given: a connected window with a machine reading and the cost of today
// When: the footer is drawn
// Then: CPU and Mem bars sit right of the status line, without Disk, and the usage button shows the cost
#[test]
fn the_footer_shows_the_machine_bars_beside_the_cost_of_today() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.harness.state_mut().stats = Some(SystemFooter::with_reading(SystemSnapshot {
        cpu_percent: 37.0,
        memory: Usage {
            used: 6 << 30,
            total: 16 << 30,
        },
        storage: None,
    }));
    let (tag, operation) = todays_summary(screen.settle());
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Diagnostics,
        tag,
        today(operation, 2_340_000),
    );
    screen.harness.run_steps(2);

    let harness = &screen.harness;
    harness.get_by_label("$2.34 today");
    let reconnect = harness.get_by_label("Reconnect").rect();
    for bar in ["CPU", "Mem"] {
        let rect = harness.get_by_label(bar).rect();
        assert!(rect.left() > reconnect.right(), "{bar} at {rect:?}");
        assert!(rect.top() > 850.0, "{bar} at {rect:?}");
    }
    assert!(harness.query_by_label("Disk").is_none());
    screen.capture("footer");
}

fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
}

// Given: each status the connection, the automatic reconnect and the restart wait can set
// When: the footer is drawn / Then: it reads in English and Reconnect stays offered
#[test]
fn the_status_line_reads_english_in_every_connection_state() {
    use crate::core_client::{auto_reconnect, connection, restart::text};
    use workbench_client::restart::reconnect::Stage;
    let budget = core_launcher::LaunchError::Budget(core_launcher::BudgetError::Ledger {
        path: "/c/.evorch-core.crash-budget".into(),
        reason: "not parseable".into(),
    });
    let incompatible = core_launcher::LaunchError::Incompatible {
        detail: "UnsupportedVersion".into(),
        advice: "Update this client.",
    };
    let statuses = [
        (true, crate::core_client::CONNECTED.to_owned()),
        (false, connection::CONNECTING.to_owned()),
        (false, connection::NOT_SENT.to_owned()),
        (false, auto_reconnect::retrying(1)),
        (false, auto_reconnect::gave_up()),
        (false, auto_reconnect::orderly_stop()),
        (false, auto_reconnect::launch_failure(&budget)),
        (false, auto_reconnect::launch_failure(&incompatible)),
        (
            false,
            auto_reconnect::launch_failure(&core_launcher::LaunchError::NotReady),
        ),
        (
            false,
            text::reconnect(Stage::RestartingToReady, Duration::from_secs(97)),
        ),
        (false, text::expired(Stage::ReadyToReconnect).to_owned()),
        (true, crate::core_client::QUEUE_FULL.to_owned()),
    ];
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    for (connected, status) in statuses {
        assert!(!status.chars().any(is_japanese), "{status}");
        {
            let mut shared = screen.shared.lock().unwrap();
            shared.connected = connected;
            shared.status.clone_from(&status);
        }
        screen.harness.run_steps(2);
        screen.harness.get_by_label(&status);
        screen.harness.get_by_label("Reconnect");
    }
}

#[test]
fn a_new_session_says_it_is_connected_in_english() {
    let mut shared = Shared::default();
    shared.begin_session(
        crate::core_client::cache::Cache::default(),
        Vec::new(),
        &[],
        Vec::new(),
    );
    assert_eq!(shared.status, "Connected to the core");
    assert!(
        shared
            .features
            .diagnostics()
            .is_some_and(|diagnostics: &DiagnosticsFeature| diagnostics.today_cost().is_none())
    );
}

// Given: a connected window
// When: ⚙ is opened
// Then: the entries are in the three groups CONFIGURE, CORE and VIEW, each with its name as
//       accessible name; the entries that open the old tabs of Providers/Routing/Agent roles
//       are one Model Settings entry; Config file is named so, not Settings; and the menu
//       is the same on a window with no Config file tab
#[test]
fn the_settings_menu_has_three_groups_of_named_entries() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label("⚙").click();
    screen.harness.run_steps(2);
    for label in [
        "CONFIGURE",
        "CORE",
        "VIEW",
        "Model Settings",
        "MCP servers",
        "Appearance",
        "Config file",
        "Restart core",
        "Usage",
        "Evaluations",
        "Panels",
        "Reset layout",
    ] {
        assert!(
            screen
                .harness
                .query_all_by_label_contains(label)
                .next()
                .is_some(),
            "{label}"
        );
    }
    for gone in [
        "Providers",
        "Routing",
        "Agent roles",
        "Settings",
        "Theme",
        "Authentication",
    ] {
        assert!(screen.harness.query_by_label(gone).is_none(), "{gone}");
    }
    screen.capture("menu");
}

// Given: a connected window whose Config file tab is not open
// When: ⚙ → Model Settings is chosen and the core answers the settings snapshot
// Then: Model Settings is shown over the window, on Providers, and no Config file tab was
//       created: the modal does not live in a tab
#[test]
fn the_settings_menu_opens_model_settings_without_a_config_file_tab() {
    use workbench_protocol::namespaces::settings::{
        SettingsQuery, SettingsReply, SettingsRevision, SettingsSnapshot,
    };
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    screen.settle();
    screen.harness.get_by_label("⚙").click();
    screen.harness.run_steps(2);
    screen.harness.get_by_label("Model Settings").click();
    screen.harness.run_steps(2);
    let tag = screen
        .settle()
        .into_iter()
        .find_map(|action| match action {
            Action::Feature(FeatureAction {
                feature: FeatureId::Settings,
                tag,
                request:
                    FeatureRequest::Query(Query::Settings(SettingsQuery::GetSettingsSnapshot {})),
            }) => Some(tag),
            _ => None,
        })
        .expect("the settings snapshot was asked");
    let dock = &screen.harness.state().dock;
    assert!(
        dock.find_tab(&CoreTab::Feature(FeatureId::Settings))
            .is_none()
    );
    let revision = SettingsRevision::new("a".repeat(64)).unwrap();
    let snapshot = SettingsSnapshot {
        revision: revision.clone(),
        applied_revision: Some(revision),
        apply_failure: None,
        document: "version = 3\n".into(),
        providers: vec![],
        problem: None,
    };
    screen.shared.lock().unwrap().features.reply(
        FeatureId::Settings,
        tag,
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
            snapshot,
        ))))),
    );
    screen.harness.run_steps(4);
    screen
        .harness
        .get_by_label("Where models come from, with their sign-in and model list");
    assert!(
        screen
            .harness
            .state()
            .dock
            .find_tab(&CoreTab::Feature(FeatureId::Settings))
            .is_none()
    );
}

mod theme {
    use super::*;

    /// The palette is process-global, so each theme test runs alone in a child process.
    fn isolated(name: &str) -> bool {
        const CHILD: &str = "EVORCH_CORE_THEME_PICKER_CHILD";
        if std::env::var_os(CHILD).is_some() {
            return false;
        }
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                &format!("core_client::shell_tests::footer::theme::{name}"),
            ])
            .env(CHILD, "1")
            .status()
            .unwrap();
        assert!(status.success());
        true
    }

    fn picker(path: Option<std::path::PathBuf>) -> Screen {
        let mut screen = screen(egui::vec2(1440.0, 900.0));
        screen.harness.state_mut().theme =
            crate::core_client::theme::ThemePicker::new(path, ThemePreset::Graphite);
        screen.settle();
        screen.harness.get_by_label("⚙").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label("Appearance").click();
        screen.harness.run_steps(2);
        screen
    }

    fn visuals_of(screen: &Screen) -> egui::Visuals {
        screen.harness.ctx.global_style().visuals.clone()
    }

    // Given: ⚙ → Theme / When: Tokyo Night is chosen, then Close
    // Then: the three presets are offered, the window turns Tokyo Night at once, and the modal closes
    #[test]
    fn the_theme_menu_previews_a_preset_live() {
        if isolated("the_theme_menu_previews_a_preset_live") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let mut screen = picker(Some(dir.path().join("ui.toml")));
        screen.harness.get_by_label("Theme settings");
        for label in ["Graphite", "Tokyo Night", "High Contrast"] {
            screen.harness.get_by_label(label);
        }
        screen.capture("theme-modal");
        screen.harness.get_by_label("Tokyo Night").click();
        screen.harness.run_steps(2);
        assert_eq!(
            visuals_of(&screen),
            style_for(ThemePreset::TokyoNight).visuals
        );
        assert_eq!(
            crate::theme::tokens::palette(),
            ThemePreset::TokyoNight.palette()
        );
        screen.harness.get_by_label("Close").click();
        screen.harness.run_steps(2);
        assert!(screen.harness.query_by_label("Theme settings").is_none());
        screen.capture("tokyo-night");
    }

    // Given: a ui.toml the embedded workbench wrote, with its own keybinds and sidebar view
    // When: Tokyo Night is chosen / Then: only the preset changes in the file
    #[test]
    fn a_chosen_preset_is_saved_and_the_rest_of_ui_toml_is_kept() {
        if isolated("a_chosen_preset_is_saved_and_the_rest_of_ui_toml_is_kept") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        let mut written = workspace_ui::UiSettings::default();
        written.keybinds.bindings.insert(
            workspace_ui::KeyAction::ResetLayout,
            "Ctrl+Shift+F9".parse().unwrap(),
        );
        written.sidebar_view.project_sort = workspace_ui::ProjectSort::Name;
        workspace_ui::save_settings(&written, &path).unwrap();

        let mut screen = picker(Some(path.clone()));
        screen.harness.get_by_label("Tokyo Night").click();
        screen.harness.run_steps(2);

        let saved = workspace_ui::load_settings(&path).unwrap();
        assert_eq!(
            saved,
            workspace_ui::UiSettings {
                theme_preset: workspace_ui::ThemePresetName::TokyoNight,
                ..written
            }
        );
        assert!(screen.harness.query_by_label(NO_SETTINGS_PATH).is_none());
        assert_eq!(
            ThemePreset::from(crate::core_client::ui_settings(dir.path()).theme_preset),
            ThemePreset::TokyoNight,
            "the next start reads the chosen preset"
        );
    }

    // Given: a ui.toml this build cannot load / When: a preset is chosen
    // Then: the file is left as it is and the error is shown, while the window still changes
    #[test]
    fn an_unreadable_ui_toml_is_not_overwritten() {
        if isolated("an_unreadable_ui_toml_is_not_overwritten") {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ui.toml");
        std::fs::write(&path, "version = \"x\"\n").unwrap();

        let mut screen = picker(Some(path.clone()));
        screen.harness.get_by_label("Tokyo Night").click();
        screen.harness.run_steps(2);

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "version = \"x\"\n");
        let error = workspace_ui::load_settings(&path).unwrap_err().to_string();
        screen.harness.get_by_label_contains(&error);
        assert_eq!(
            visuals_of(&screen),
            style_for(ThemePreset::TokyoNight).visuals
        );
    }

    // Given: a window started without a settings path / When: a preset is chosen
    // Then: the embedded workbench's "not saved" note is shown
    #[test]
    fn without_a_settings_path_the_theme_is_not_saved() {
        if isolated("without_a_settings_path_the_theme_is_not_saved") {
            return;
        }
        let mut screen = picker(None);
        screen.harness.get_by_label("High Contrast").click();
        screen.harness.run_steps(2);
        screen.harness.get_by_label(NO_SETTINGS_PATH);
        assert_eq!(
            visuals_of(&screen),
            style_for(ThemePreset::HighContrast).visuals
        );
    }
}
