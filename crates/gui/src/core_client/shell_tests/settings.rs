//! The settings modal and the Authentication tab in the full window: each ⚙ entry opens the
//! modal on its tab over the core's snapshot, and the text reads in English.
use super::*;
use egui_kittest::kittest::NodeT;
use workbench_protocol::namespaces::{
    auth::{AuthQuery, AuthReply, AuthStatus, CredentialSlot, CredentialState, LoginMethod},
    settings::{
        CredentialRef, ModelView, ProviderKind, ProviderView, SettingsQuery, SettingsReply,
        SettingsRevision, SettingsSnapshot,
    },
};

const DOCUMENT: &str = r#"version = 3
[providers.local]
type = "openai-compatible"
base_url = "http://localhost:11434/v1"
api_key_env = "LOCAL_KEY"
models = ["qwen3-coder", "gpt-oss-20b"]
default_model = "qwen3-coder"

[providers.codex]
type = "openai-codex"
api_protocol = "openai-codex-responses"
base_url = "https://chatgpt.com/backend-api/codex"
credential = { type = "keyring", service = "evorch", account = "codex" }
models = ["gpt-5.2", "gpt-5.3-codex"]
default_model = "gpt-5.3-codex"

[providers.claude]
type = "anthropic-subscription"
credential = { type = "keyring", service = "evorch", account = "claude" }
models = ["claude-sonnet-5-5"]
default_model = "claude-sonnet-5-5"

[routing.routes]
worker = [{ profile = "claude", model = "claude-sonnet-5-5" }, { profile = "codex" }]
fast = [{ profile = "local", model = "qwen3-coder" }]

[agents.explorer]
models = [{ route = "fast" }]
"#;

fn provider(
    name: &str,
    kind: ProviderKind,
    models: &[&str],
    credential: CredentialRef,
) -> ProviderView {
    ProviderView {
        name: name.into(),
        kind,
        base_url: String::new(),
        credential,
        models: models
            .iter()
            .map(|id| ModelView {
                id: (*id).into(),
                enabled: true,
            })
            .collect(),
        excluded_models: vec![],
        default_model: models[0].into(),
    }
}

fn keyring(account: &str) -> CredentialRef {
    CredentialRef::Keyring {
        service: "evorch".into(),
        account: account.into(),
    }
}

fn snapshot() -> SettingsSnapshot {
    let revision = SettingsRevision::new("a".repeat(64)).unwrap();
    SettingsSnapshot {
        revision: revision.clone(),
        applied_revision: Some(revision),
        apply_failure: None,
        document: DOCUMENT.into(),
        providers: vec![
            ProviderView {
                base_url: "http://localhost:11434/v1".into(),
                ..provider(
                    "local",
                    ProviderKind::OpenaiCompatible,
                    &["qwen3-coder", "gpt-oss-20b"],
                    CredentialRef::Env {
                        var: "LOCAL_KEY".into(),
                    },
                )
            },
            provider(
                "codex",
                ProviderKind::OpenaiCodex,
                &["gpt-5.3-codex", "gpt-5.2"],
                keyring("codex"),
            ),
            provider(
                "claude",
                ProviderKind::AnthropicSubscription,
                &["claude-sonnet-5-5"],
                keyring("claude"),
            ),
            provider(
                "api",
                ProviderKind::Anthropic,
                &["claude-opus-4-7"],
                CredentialRef::Env {
                    var: "ANTHROPIC_API_KEY".into(),
                },
            ),
        ],
        problem: None,
    }
}

fn is_japanese(c: char) -> bool {
    matches!(c, '\u{3000}'..='\u{30ff}' | '\u{4e00}'..='\u{9fff}' | '\u{ff00}'..='\u{ffef}')
}

fn japanese_texts(screen: &Screen) -> std::collections::BTreeSet<String> {
    screen
        .harness
        .query_all_by(|node| {
            [node.label(), node.value()]
                .into_iter()
                .flatten()
                .any(|text| text.chars().any(is_japanese))
        })
        .map(|node| {
            let node = node.accesskit_node();
            format!("{:?} {:?}", node.label(), node.value())
        })
        .collect()
}

fn assert_english(screen: &Screen, before: &std::collections::BTreeSet<String>) {
    let added: Vec<_> = japanese_texts(screen).difference(before).cloned().collect();
    assert!(added.is_empty(), "{added:?}");
}

fn open(
    screen: &mut Screen,
    entry: &str,
    answer: impl Fn(&FeatureAction) -> Option<FeatureReply>,
) -> std::collections::BTreeSet<String> {
    screen.settle();
    let before = japanese_texts(screen);
    screen.harness.get_by_label("⚙").click();
    screen.harness.run_steps(2);
    match screen.harness.query_by_label(entry) {
        Some(item) => item.click(),
        None => screen.harness.get_by_label_contains(entry).click(),
    }
    screen.harness.run_steps(2);
    let replies: Vec<_> = screen
        .settle()
        .into_iter()
        .filter_map(|action| match action {
            Action::Feature(action) => {
                answer(&action).map(|reply| (action.feature, action.tag, reply))
            }
            _ => None,
        })
        .collect();
    assert!(!replies.is_empty(), "{entry} asked nothing");
    {
        let mut shared = screen.shared.lock().unwrap();
        for (feature, tag, reply) in replies {
            shared.features.reply(feature, tag, reply);
        }
    }
    // The modal sizes itself over its first frames before it takes clicks.
    screen.harness.run_steps(8);
    before
}

fn settings_snapshot(action: &FeatureAction) -> Option<FeatureReply> {
    matches!(
        action.request,
        FeatureRequest::Query(Query::Settings(SettingsQuery::GetSettingsSnapshot {}))
    )
    .then(|| {
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
            snapshot(),
        )))))
    })
}

/// ⚙ → Model Settings, then the section `section` of its navigation.
fn settings(section: &str) -> (Screen, std::collections::BTreeSet<String>) {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let before = open(&mut screen, "Model Settings", settings_snapshot);
    if section != "Providers" {
        screen.harness.get_by_label(section).click();
        screen.harness.run_steps(8);
    }
    (screen, before)
}

// Given: a core whose file has four providers of four types
// When: ⚙ → Model Settings
// Then: the modal lists each with its type badge and default model, Edit and Delete, and
//       the legacy add buttons; Delete asks before anything is sent
#[test]
fn the_providers_tab_shows_type_badges_and_confirms_a_delete() {
    let (mut screen, before) = settings("Providers");
    let harness = &screen.harness;
    harness.get_by_label("Where models come from, with their sign-in and model list");
    for label in [
        "OpenAI-compatible",
        "Codex subscription",
        "Claude subscription",
        "Claude API",
        "qwen3-coder",
        "gpt-5.3-codex",
        "+ Add OpenAI-compatible",
        "+ Add Codex subscription",
        "+ Add Claude subscription",
        "+ Add Cursor",
        "+ Add Kimi subscription",
    ] {
        harness.get_by_label(label);
    }
    assert_eq!(harness.get_all_by_label("Delete").count(), 4);
    assert_english(&screen, &before);
    screen.capture("providers");

    screen
        .harness
        .get_all_by_label("Delete")
        .nth(1)
        .unwrap()
        .click();
    screen.harness.run_steps(4);
    screen.harness.get_by_label("Delete codex?");
    screen.harness.get_by_label("Confirm delete");
    screen.harness.get_by_label("Keep profile");
    assert!(
        screen.settle().is_empty(),
        "nothing is sent before the confirmation"
    );
    screen.capture("delete-confirm");

    screen.harness.get_by_label("Keep profile").click();
    screen.harness.run_steps(4);
    assert!(screen.harness.query_by_label("Delete codex?").is_none());
    screen
        .harness
        .get_all_by_label("Edit")
        .next()
        .unwrap()
        .click();
    screen.harness.run_steps(8);
    for label in [
        "Name",
        "Base URL",
        "Keyring (recommended)",
        "Environment variable",
        "API key env var",
        "Excluded models",
        "Default model",
        "Save",
        "Back to list",
    ] {
        screen.harness.get_by_label(label);
    }
    assert!(
        screen.settle().is_empty(),
        "opening the editor sends nothing"
    );
    assert_english(&screen, &before);
    screen.capture("provider-editor");
}

// Given: a file with two routes / When: ⚙ → Model Settings → Routing
// Then: the legacy routing editor shows the routes of the file
#[test]
fn the_routing_tab_shows_the_routes_of_the_file() {
    let (mut screen, before) = settings("Routing");
    let harness = &screen.harness;
    harness.get_by_label(
        "Logical model name to real provider and model. Candidates are tried from the top",
    );
    harness.get_by_label("Save routing");
    assert_english(&screen, &before);
    screen.capture("routing");
}

// Given: a file binding the explorer to a route / When: ⚙ → Model Settings → Agent Roles
// Then: the legacy role editor is shown with its Save
#[test]
fn the_agent_roles_tab_shows_the_roles_of_the_file() {
    let (mut screen, before) = settings("Agent Roles");
    screen
        .harness
        .get_by_label("Which route each agent role uses, and how it is tuned");
    assert_english(&screen, &before);
    screen.capture("roles");
}

fn auth_status(action: &FeatureAction) -> Option<FeatureReply> {
    let FeatureRequest::Query(Query::Auth(AuthQuery::GetCredentialStatus {})) = action.request
    else {
        return None;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let slot = |profile: &str, kind, storage, state, login| CredentialSlot {
        profile: profile.into(),
        kind,
        storage,
        state,
        login,
    };
    Some(FeatureReply::Query(Ok(QueryReply::Auth(
        AuthReply::Status(AuthStatus {
            revision: Revision(1),
            slots: vec![
                slot(
                    "codex",
                    ProviderKind::OpenaiCodex,
                    keyring("codex"),
                    CredentialState::Present {
                        expires_at_unix: Some(now + 5 * 3600 + 1800),
                    },
                    Some(LoginMethod::Codex),
                ),
                slot(
                    "claude",
                    ProviderKind::AnthropicSubscription,
                    keyring("claude"),
                    CredentialState::Present {
                        expires_at_unix: Some(now.saturating_sub(600)),
                    },
                    Some(LoginMethod::Claude),
                ),
                slot(
                    "kimi",
                    ProviderKind::KimiSubscription,
                    keyring("kimi"),
                    CredentialState::Present {
                        expires_at_unix: None,
                    },
                    None,
                ),
                slot(
                    "local",
                    ProviderKind::OpenaiCompatible,
                    CredentialRef::Env {
                        var: "LOCAL_KEY".into(),
                    },
                    CredentialState::Missing,
                    None,
                ),
                slot(
                    "cursor",
                    ProviderKind::Cursor,
                    keyring("cursor"),
                    CredentialState::Missing,
                    Some(LoginMethod::Cursor),
                ),
            ],
        }),
    ))))
}

// Given: sign-ins that expire later, expired, and keys stored or missing
// When: ⚙ → Authentication
// Then: each profile shows its type badge and a readable expiry, in English
#[test]
fn the_authentication_tab_reads_expiry_and_types_in_english() {
    let mut screen = screen(egui::vec2(1440.0, 900.0));
    let before = open(&mut screen, "Authentication", auth_status);
    let harness = &screen.harness;
    harness.get_by_label_contains("Signed in · token expires in 5h");
    harness.get_by_label("Signed in · token expired");
    harness.get_by_label("Token expired; it will renew automatically when used.");
    harness.get_by_label("Stored");
    harness.get_by_label("Sign in to Codex");
    harness.get_by_label("Read from environment variable LOCAL_KEY.");
    for badge in [
        "Codex subscription",
        "Claude subscription",
        "Kimi subscription",
        "OpenAI-compatible",
        "Cursor",
    ] {
        harness.get_by_label(badge);
    }
    assert_english(&screen, &before);
    screen.capture("auth");
}

// Given: the smallest supported window, 960×600
// When: Model Settings is opened on each of its sections
// Then: the × and the navigation are inside the window, and the modal's body scrolls rather
//       than pushing anything out of it
#[test]
fn model_settings_keeps_its_controls_in_a_minimum_window() {
    let size = egui::vec2(960.0, 600.0);
    let screen_rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
    for section in ["Providers", "Routing", "Agent Roles"] {
        let mut screen = screen(size);
        let _ = open(&mut screen, "Model Settings", settings_snapshot);
        if section != "Providers" {
            screen.harness.get_by_label(section).click();
        }
        screen.harness.run_steps(8);
        for label in ["Close", "Providers", "Routing", "Agent Roles"] {
            let nodes: Vec<_> = screen.harness.get_all_by_label(label).collect();
            assert!(!nodes.is_empty(), "{label} on {section}");
            for node in nodes {
                let rect = node.rect();
                assert!(
                    screen_rect.contains_rect(rect),
                    "{label} on {section}: {rect:?} outside {screen_rect:?}"
                );
            }
        }
        if section == "Providers" {
            screen.capture("minimum");
        }
    }
}
