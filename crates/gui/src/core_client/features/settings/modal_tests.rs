//! The settings modal drawn headless: each tab saves the whole document against the revision
//! its draft was made from, keeps what it does not edit, and waits for the core's next
//! snapshot when the file moved under it.
use super::modal::STALE_DRAFT;
use super::tests::{open_modal, provider, provider_save, revision, settings_command, view};
use super::*;
use crate::core_client::features::FeatureRequest;
use crate::core_client::features::execution::tests::{Screen, answer, asked, query_of, screen};
use crate::model::settings_document::SettingsDocument;
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::namespaces::settings::{
    CredentialRef, OpenaiCompatibleKind, ProviderInput, ProviderKind,
};

const DOCUMENT: &str = r#"version = 3
# the person's note
[core]
idle_grace_secs = 123

[core.restart.targets.next]
path = "/local/core"

[providers.local]
type = "openai-compatible"
base_url = "http://localhost:1/v1"
api_key_env = "KEY"
models = ["m"]
default_model = "m"

[routing.routes]
fast = [{ profile = "local" }]

[agents.explorer]
models = [{ route = "fast" }]
"#;

const KEPT: [&str; 4] = [
    "# the person's note",
    "idle_grace_secs = 123",
    "/local/core",
    "[providers.local]",
];

fn snapshot_reply(saved: char, document: &str, problem: Option<SettingsProblem>) -> FeatureReply {
    let mut api = provider("api");
    api.kind = ProviderKind::Anthropic;
    FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
        SettingsSnapshot {
            revision: revision(saved),
            applied_revision: Some(revision(saved)),
            apply_failure: None,
            document: document.into(),
            providers: vec![provider("local"), api],
            problem,
        },
    )))))
}

fn loaded_with(document: &str, problem: Option<SettingsProblem>) -> Harness<'static, Screen> {
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    answer(
        &mut screen,
        first[0].tag,
        snapshot_reply('a', document, problem),
    );
    screen
}

fn answer_snapshot(screen: &mut Harness<'static, Screen>, saved: char, document: &str) {
    let sent = asked(screen);
    let query = sent
        .iter()
        .find(|action| {
            matches!(
                query_of(action),
                Query::Settings(SettingsQuery::GetSettingsSnapshot {})
            )
        })
        .expect("the snapshot is asked for again");
    answer(screen, query.tag, snapshot_reply(saved, document, None));
    screen.run();
}

fn type_into(screen: &mut Harness<'static, Screen>, label: &str, text: &str) {
    screen
        .get_by_role_and_label(Role::TextInput, label)
        .scroll_to_me();
    screen.run();
    screen.get_by_role_and_label(Role::TextInput, label).focus();
    screen.run();
    screen
        .get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    screen.run();
}

/// Presses the control named `label`, scrolled into view first: the section's body scrolls.
fn click(screen: &mut Harness<'static, Screen>, label: &str) {
    screen.get_by_label(label).scroll_to_me();
    screen.run();
    screen.get_by_label(label).click();
    screen.run();
}

fn document_save(screen: &mut Harness<'static, Screen>) -> SaveDocument {
    let sent = asked(screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
        panic!("a document save was expected");
    };
    save
}

fn end(screen: &mut Harness<'static, Screen>, save: &SaveDocument, state: OperationState) {
    let mut ended = view(
        &save.operation_id,
        SettingsOperationKind::SaveDocument,
        state,
    );
    if matches!(state, OperationState::Ready) {
        ended.revision = Some(revision('e'));
    }
    screen.state_mut().feature.event(&UiEvent::Settings(
        SettingsEvent::SettingsOperationCompleted { operation: ended },
    ));
    screen.run();
}

/// Adds a route through the pinned Add button and names it: the name field of a new route
/// is open and selected, so what is typed replaces the placeholder.
fn add_route(screen: &mut Harness<'static, Screen>, name: &str) {
    click(screen, "Add route");
    type_into(screen, "Logical model name", name);
    click(screen, "Accept name");
}

/// Opens the card of the route `name`.
fn open_route(screen: &mut Harness<'static, Screen>, name: &str) {
    screen.get_by_label(&format!("Route {name}")).click();
    screen.run();
}

/// Picks the role named `label` in the table (its row is a button).
fn pick_role(screen: &mut Harness<'static, Screen>, label: &str) {
    screen
        .get_by_role_and_label(Role::Button, label)
        .scroll_to_me();
    screen.run();
    screen.get_by_role_and_label(Role::Button, label).click();
    screen.run();
}

/// Turns the Advanced Temperature override of the selected role on: a small edit that makes
/// the role unsaved.
fn edit_selected_role(screen: &mut Harness<'static, Screen>) {
    screen
        .query_all_by_label_contains("ADVANCED")
        .next()
        .expect("the Advanced header")
        .click();
    screen.run();
    click(screen, "Temperature");
}

// Given: two providers, one of a type this screen cannot edit
// When: the Providers tab is opened, and Delete is pressed, kept, then pressed and confirmed
// Then: each row names its type, the other type cannot be edited here, and nothing is sent
//       until the confirmation, which deletes that profile at the snapshot revision
#[test]
fn a_provider_row_names_its_type_and_is_deleted_only_after_its_confirmation() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Providers");
    screen.get_by_label("Where models come from, with their sign-in and model list");
    screen.get_by_label("OpenAI-compatible");
    screen.get_by_label("Claude API");
    screen.get_all_by_label("Edit").nth(1).unwrap().click();
    screen.run();
    assert!(screen.query_by_label("Back to list").is_none());

    let delete_local = |screen: &mut Harness<'static, Screen>| {
        screen.get_all_by_label("Delete").next().unwrap().click();
        screen.run();
    };
    delete_local(&mut screen);
    assert!(asked(&mut screen).is_empty());
    screen.get_by_label("Delete local?");
    click(&mut screen, "Keep profile");
    assert!(asked(&mut screen).is_empty());
    assert!(screen.query_by_label("Confirm delete").is_none());

    delete_local(&mut screen);
    click(&mut screen, "Confirm delete");
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let SettingsCommand::DeleteProvider(delete) = settings_command(&sent[0]) else {
        panic!("a provider delete was expected");
    };
    assert_eq!(delete.expected_revision, revision('a'));
    assert_eq!(delete.name, "local");
}

// Given: a provider delete sent at the revision this screen holds
// When: the core refuses it as stale
// Then: the modal says so and asks the core to re-read the file, once
#[test]
fn a_stale_provider_delete_asks_the_core_to_re_read_the_file() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Providers");
    screen.get_all_by_label("Delete").next().unwrap().click();
    screen.run();
    click(&mut screen, "Confirm delete");
    let sent = asked(&mut screen);
    let SettingsCommand::DeleteProvider(delete) = settings_command(&sent[0]).clone() else {
        panic!("a provider delete was expected");
    };
    screen.state_mut().feature.event(&UiEvent::Settings(
        SettingsEvent::SettingsOperationCompleted {
            operation: view(
                &delete.operation_id,
                SettingsOperationKind::DeleteProvider,
                OperationState::Failed {
                    code: ErrorCode::StaleRevision,
                },
            ),
        },
    ));
    screen.run();
    screen.get_by_label(STALE_REREAD);
    let refreshes = asked(&mut screen)
        .iter()
        .filter(|action| {
            matches!(
                &action.request,
                FeatureRequest::Command(Command::Settings(SettingsCommand::RefreshSettings(_)))
            )
        })
        .count();
    assert_eq!(refreshes, 1);
}

// Given: a screen whose first settings snapshot has not arrived
// When: the Routing tab of the modal is opened
// Then: it says the settings are loading, not that the tab could not be loaded
#[test]
fn a_tab_opened_before_the_first_snapshot_says_it_is_loading() {
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    let _ = asked(&mut screen);
    settings(&mut screen).open_modal(SettingsTab::Routing);
    screen.run();
    screen.get_by_label("Loading settings…");
}

// Given: the Providers tab
// When: a Kimi subscription is added and saved as prefilled
// Then: one new OpenAI-compatible provider of the Kimi kind is sent, named after its type,
//       with its endpoint and models filled in and its key named in the keyring
#[test]
fn a_kimi_subscription_is_added_with_its_defaults() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Providers");
    click(&mut screen, "+ Add Kimi subscription");
    click(&mut screen, "Save");
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let save = provider_save(&sent[0]);
    assert_eq!(save.expected_revision, revision('a'));
    assert_eq!(save.original_name, None);
    let ProviderInput::OpenaiCompatible {
        name,
        kind,
        base_url,
        credential,
        models,
        default_model,
        ..
    } = save.provider
    else {
        panic!("an OpenAI-compatible input was expected");
    };
    assert_eq!(kind, OpenaiCompatibleKind::KimiSubscription);
    assert_eq!(name, "kimi");
    assert!(!base_url.is_empty());
    assert!(!models.is_empty());
    assert!(models.iter().any(|model| model.id == default_model));
    assert_eq!(
        credential,
        CredentialRef::Keyring {
            service: "evorch".into(),
            account: "kimi".into(),
        }
    );
}

// Given: the Routing tab of a file with comments, restart targets and providers
// When: a route is added and the routing saved, and the save ends
// Then: the document is sent at the snapshot revision with the route and everything else it
//       had, and the end names the route and asks for the reload
#[test]
fn a_routing_save_quotes_its_revision_and_keeps_the_rest_of_the_file() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    screen.get_by_label(
        "Logical model name to real provider and model. Candidates are tried from the top",
    );
    add_route(&mut screen, "slow");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert_eq!(save.expected_revision, revision('a'));
    for kept in KEPT {
        assert!(save.text.contains(kept), "{kept} in {}", save.text);
    }
    assert!(save.text.contains("slow"));

    end(&mut screen, &save, OperationState::Ready);
    screen.get_by_label("Route 'slow' saved. Reload models to apply it.");
    answer_snapshot(&mut screen, 'e', &save.text);
    assert!(screen.query_by_label(STALE_DRAFT).is_none());
}

// Given: a routing draft with an added route
// When: its save is refused as stale while the core still holds the refused revision, and
//       the file the core then re-reads changed only a provider
// Then: the modal asks the core to re-read the file, keeps the draft unsaved until the newer
//       snapshot arrives, then rebases it so its save quotes the new revision and keeps the
//       provider change
#[test]
fn a_stale_routing_save_is_rebased_onto_a_file_whose_routes_did_not_change() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "slow");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    end(
        &mut screen,
        &save,
        OperationState::Failed {
            code: ErrorCode::StaleRevision,
        },
    );
    screen.get_by_label(STALE_DRAFT);
    let sent = asked(&mut screen);
    let refresh = sent
        .iter()
        .find_map(|action| match &action.request {
            FeatureRequest::Command(Command::Settings(SettingsCommand::RefreshSettings(
                refresh,
            ))) => Some(refresh.operation_id.clone()),
            _ => None,
        })
        .expect("the core is asked to re-read the file");
    let cached = sent
        .iter()
        .find(|action| {
            matches!(
                &action.request,
                FeatureRequest::Query(Query::Settings(SettingsQuery::GetSettingsSnapshot {}))
            )
        })
        .expect("the snapshot is asked for again");
    answer(&mut screen, cached.tag, snapshot_reply('a', DOCUMENT, None));
    screen.run();
    screen.get_by_label(STALE_DRAFT);
    click(&mut screen, "Save routing");
    assert!(asked(&mut screen).is_empty());

    screen.state_mut().feature.event(&UiEvent::Settings(
        SettingsEvent::SettingsOperationCompleted {
            operation: view(
                &refresh,
                SettingsOperationKind::Refresh,
                OperationState::Ready,
            ),
        },
    ));
    screen.run();
    screen.get_by_label(STALE_REREAD);
    let elsewhere = DOCUMENT.replace("localhost:1", "localhost:2");
    answer_snapshot(&mut screen, 'c', &elsewhere);
    assert!(screen.query_by_label(STALE_DRAFT).is_none());

    click(&mut screen, "Save routing");
    let again = document_save(&mut screen);
    assert_eq!(again.expected_revision, revision('c'));
    assert!(again.text.contains("localhost:2"));
    assert!(again.text.contains("slow"));
}

// Given: a draft of the route `fast` (a candidate added), and a draft of a new route
// When: the file changes elsewhere, in `fast` itself
// Then: `fast` is flagged as changed elsewhere and Save sends nothing for it, while Discard
//       takes the saved route; the new route stays a draft, and saves
#[test]
fn a_route_changed_elsewhere_under_unsaved_edits_is_not_saved_over() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "slow");
    open_route(&mut screen, "fast");
    click(&mut screen, "Add candidate");
    snapshot_changed(&mut screen, 'c');
    let elsewhere = DOCUMENT.replace(
        "fast = [{ profile = \"local\" }]",
        "fast = [{ profile = \"local\", model = \"m\" }]",
    );
    answer_snapshot(&mut screen, 'c', &elsewhere);
    screen.get_by_label("Changed elsewhere");
    click(&mut screen, "Save routing");
    assert!(asked(&mut screen).is_empty());

    click(&mut screen, "Discard");
    assert!(screen.query_by_label("Changed elsewhere").is_none());
    open_route(&mut screen, "slow");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert_eq!(save.expected_revision, revision('c'));
    assert!(save.text.contains("slow"));
    assert!(save.text.contains("model = \"m\""), "{}", save.text);
}

// Given: a new route whose name was typed but not accepted
// When: Save routing is pressed
// Then: the typed name is what is saved, not the placeholder
#[test]
fn save_takes_the_name_still_being_typed() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    click(&mut screen, "Add route");
    type_into(&mut screen, "Logical model name", "typed-name");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert!(save.text.contains("typed-name"), "{}", save.text);
    assert!(!save.text.contains("new-route"), "{}", save.text);
}

// Given: unsaved drafts of two routes
// When: the first is saved and its end seen
// Then: the second is still unsaved, is not in conflict, and saves at the revision the first
//       wrote, with the first still in the file
#[test]
fn saving_one_route_leaves_the_other_draft_saveable() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "first");
    add_route(&mut screen, "second");
    open_route(&mut screen, "first");
    click(&mut screen, "Save routing");
    let first = document_save(&mut screen);
    assert!(first.text.contains("first") && !first.text.contains("second"));
    end(&mut screen, &first, OperationState::Ready);
    answer_snapshot(&mut screen, 'e', &first.text);
    screen.get_by_label("● 1 other route unsaved");
    assert!(screen.query_by_label("Changed elsewhere").is_none());

    open_route(&mut screen, "second");
    click(&mut screen, "Save routing");
    let second = document_save(&mut screen);
    assert_eq!(second.expected_revision, revision('e'));
    assert!(second.text.contains("first") && second.text.contains("second"));
}

// Given: the Agent roles tab
// When: the roles are saved, then a profile is created from the selected one and deleted
// Then: each save is the whole document at the revision the previous one wrote, keeping
//       the rest of the file, and each end names what changed
#[test]
fn agent_roles_and_role_profiles_are_saved_at_the_revision_they_were_made_from() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Agent roles");
    screen.get_by_label("Which route each agent role uses, and how it is tuned");
    edit_selected_role(&mut screen);
    click(&mut screen, "Save role");
    let roles = document_save(&mut screen);
    assert_eq!(roles.expected_revision, revision('a'));
    for kept in KEPT.iter().chain(&["fast"]) {
        assert!(roles.text.contains(kept), "{kept} in {}", roles.text);
    }
    end(&mut screen, &roles, OperationState::Ready);
    screen.get_by_label("Orchestrator saved. Reload models to apply it.");
    answer_snapshot(&mut screen, 'e', &roles.text);

    click(&mut screen, "Profile menu");
    type_into(&mut screen, "New profile name", "night");
    click(&mut screen, "New profile");
    let created = document_save(&mut screen);
    assert_eq!(created.expected_revision, revision('e'));
    assert!(created.text.contains("night"));
    for kept in KEPT {
        assert!(created.text.contains(kept), "{kept} in {}", created.text);
    }
    end(&mut screen, &created, OperationState::Ready);
    screen.get_by_label("Role profile 'night' created. Reload models to apply it.");
    answer_snapshot(&mut screen, 'e', &created.text);

    click(&mut screen, "Profile menu");
    click(&mut screen, "Delete profile");
    let deleted = document_save(&mut screen);
    assert!(!deleted.text.contains("night"));
    for kept in KEPT {
        assert!(deleted.text.contains(kept), "{kept} in {}", deleted.text);
    }
}

// Given: a snapshot whose document is too large to send, and one that is not valid TOML
// When: the Routing and the Agent roles tabs are opened
// Then: each names why it cannot be edited and offers no save
#[test]
fn an_unusable_document_is_named_and_offers_no_save() {
    let mut screen = loaded_with("", Some(SettingsProblem::DocumentTooLarge));
    open_modal(&mut screen, "Routing");
    screen.get_by_label("The settings file is too large to edit here.");
    assert!(screen.query_by_label("Save routing").is_none());
    assert!(asked(&mut screen).is_empty());
    drop(screen);

    let mut screen = loaded_with("version = [", None);
    open_modal(&mut screen, "Agent roles");
    assert!(
        screen
            .query_all_by_label_contains("The settings file cannot be edited here")
            .next()
            .is_some()
    );
    assert!(screen.query_by_label("Save role").is_none());
    assert!(asked(&mut screen).is_empty());
}

// Given: the Agent roles tab, whose roles name routes that do not exist
// When: Create route is pressed and the routing saved
// Then: the Routing tab opens with the route of that role, and the finished save returns to
//       Agent roles
#[test]
fn create_route_opens_routing_and_returns_to_agent_roles_after_the_save() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Agent roles");
    click(&mut screen, "Create route");
    screen.get_by_label(
        "Logical model name to real provider and model. Candidates are tried from the top",
    );
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert_eq!(save.expected_revision, revision('a'));
    assert!(save.text.contains("orchestrator"), "{}", save.text);
    end(&mut screen, &save, OperationState::Ready);
    screen.get_by_label("Which route each agent role uses, and how it is tuned");
}

fn snapshot_changed(screen: &mut Harness<'static, Screen>, saved: char) {
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Settings(SettingsEvent::SettingsSnapshotChanged {
            revision: revision(saved),
            applied_revision: Some(revision('a')),
        }));
}

// Given: a routing draft with an added route, in a file without a cooldown
// When: another client adds a cooldown, which leaves the routes as they were
// Then: the draft is rebased, and its save quotes the new revision and keeps that cooldown
#[test]
fn a_rebased_routing_draft_keeps_a_cooldown_set_elsewhere() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "slow");
    snapshot_changed(&mut screen, 'c');
    let elsewhere =
        format!("{DOCUMENT}\n[routing.cooldown]\nquota_secs = 600\nrate_limit_secs = 30\n");
    answer_snapshot(&mut screen, 'c', &elsewhere);
    assert!(screen.query_by_label("Changed elsewhere").is_none());

    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert_eq!(save.expected_revision, revision('c'));
    assert!(save.text.contains("quota_secs = 600"), "{}", save.text);
    assert!(save.text.contains("slow"));
}

// Given: a routing draft with an added route, saved at the snapshot revision
// When: the newer snapshot, with the routes kept, arrives before the save is refused as stale
// Then: the refusal leaves the rebased draft saveable, and its next save quotes the new
//       revision
#[test]
fn a_stale_refusal_seen_after_the_rebase_does_not_lock_the_draft() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "slow");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    snapshot_changed(&mut screen, 'c');
    let elsewhere = DOCUMENT.replace("localhost:1", "localhost:2");
    answer_snapshot(&mut screen, 'c', &elsewhere);
    end(
        &mut screen,
        &save,
        OperationState::Failed {
            code: ErrorCode::StaleRevision,
        },
    );
    answer_snapshot(&mut screen, 'c', &elsewhere);
    assert!(screen.query_by_label(STALE_DRAFT).is_none());

    click(&mut screen, "Save routing");
    let again = document_save(&mut screen);
    assert_eq!(again.expected_revision, revision('c'));
    assert!(again.text.contains("localhost:2"));
    assert!(again.text.contains("slow"));
}

fn settings<'a>(screen: &'a mut Harness<'static, Screen>) -> &'a mut SettingsFeature {
    screen
        .state_mut()
        .feature
        .as_settings_mut()
        .expect("the settings feature")
}

// Given: an Agent roles draft with unsaved edits bound to route `fast`, and a routing save
//       that renames `fast` to `faster`
// When: a reconnect asks for the snapshot the save wrote before the save's own end
// Then: the renamed route is flagged until the save's end is seen; the end clears it, the
//       roles draft carries the rename and its save quotes the revision the rename wrote
#[test]
fn a_rename_seen_in_the_snapshot_before_its_own_save_ends_leaves_agent_roles_saveable() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Agent roles");
    screen.get_by_label("Which route each agent role uses, and how it is tuned");
    {
        let modal = settings(&mut screen).modal.as_mut().expect("the modal");
        let roles = modal.roles.as_mut().expect("the roles draft");
        roles.model.agents.orchestrator = roles.model.agents.explorer.clone();
        modal.select(SettingsTab::Routing);
    }
    screen.run();
    open_route(&mut screen, "fast");
    click(&mut screen, "Rename route fast");
    type_into(&mut screen, "Logical model name", "faster");
    click(&mut screen, "Accept name");
    click(&mut screen, "Save routing");
    let save = document_save(&mut screen);
    assert!(save.text.contains("route = \"faster\""), "{}", save.text);

    settings(&mut screen).reset();
    screen.run();
    let sent = asked(&mut screen);
    let tag_of = |snapshot: bool| {
        sent.iter()
            .find(|action| match &action.request {
                FeatureRequest::Query(Query::Settings(SettingsQuery::GetSettingsSnapshot {})) => {
                    snapshot
                }
                FeatureRequest::Query(Query::Settings(SettingsQuery::GetSettingsOperation {
                    ..
                })) => !snapshot,
                _ => false,
            })
            .expect("the snapshot and the operation are asked for")
            .tag
    };
    let (snapshot_tag, operation_tag) = (tag_of(true), tag_of(false));
    answer(
        &mut screen,
        snapshot_tag,
        snapshot_reply('e', &save.text, None),
    );
    screen.run();
    let flagged = |screen: &mut Harness<'static, Screen>| {
        settings(screen)
            .modal
            .as_ref()
            .and_then(|modal| modal.routing.as_ref())
            .is_some_and(|routing| !routing.model.conflicted.is_empty())
    };
    assert!(flagged(&mut screen));

    let mut ended = view(
        &save.operation_id,
        SettingsOperationKind::SaveDocument,
        OperationState::Ready,
    );
    ended.revision = Some(revision('e'));
    answer(
        &mut screen,
        operation_tag,
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Operation(ended)))),
    );
    screen.run();
    let _ = asked(&mut screen);
    assert!(!flagged(&mut screen));
    settings(&mut screen)
        .modal
        .as_mut()
        .expect("the modal")
        .select(SettingsTab::AgentRoles);
    screen.run();
    click(&mut screen, "Save role");
    let roles = document_save(&mut screen);
    assert_eq!(roles.expected_revision, revision('e'));
    assert!(!roles.text.contains("\"fast\""), "{}", roles.text);
    assert!(roles.text.contains("faster"), "{}", roles.text);
}

// Given: a Claude subscription whose sign-in is kept under the keychain account of its name
// When: it is edited, renamed and saved
// Then: the save keeps the account, so the stored sign-in stays with the provider
#[test]
fn renaming_an_existing_subscription_keeps_its_keychain_account() {
    let mut claude = provider("claude");
    claude.kind = ProviderKind::AnthropicSubscription;
    claude.credential = CredentialRef::Keyring {
        service: "evorch".into(),
        account: "claude".into(),
    };
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    let first = asked(&mut screen);
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
            SettingsSnapshot {
                revision: revision('a'),
                applied_revision: Some(revision('a')),
                apply_failure: None,
                document: DOCUMENT.into(),
                providers: vec![claude],
                problem: None,
            },
        ))))),
    );
    open_modal(&mut screen, "Providers");
    click(&mut screen, "Edit");
    type_into(&mut screen, "Name", "-work");
    click(&mut screen, "Save");
    let sent = asked(&mut screen);
    let ProviderInput::Subscription { name, account, .. } = provider_save(&sent[0]).provider else {
        panic!("a subscription input was expected");
    };
    assert_ne!(name, "claude");
    assert_eq!(account, "claude");
}

// Given: Model Settings open on Routing with nothing edited
// When: Esc is pressed
// Then: it closes at once, without asking
#[test]
fn escape_closes_a_clean_modal_at_once() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    assert!(settings(&mut screen).modal_tab().is_some());
    screen.key_press(egui::Key::Escape);
    screen.run();
    assert!(settings(&mut screen).modal_tab().is_none());
}

// Given: a Routing draft with an added route
// When: × is pressed, "Keep editing" is chosen, then Esc and "Discard and close"
// Then: the first request only asks and keeps the draft; discarding closes the modal and sends
//       nothing
#[test]
fn closing_a_dirty_modal_asks_first_and_discarding_sends_nothing() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Routing");
    add_route(&mut screen, "slow");
    screen.get_by_label("● Unsaved changes: slow");

    screen.get_by_label("Close").click();
    screen.run();
    screen.get_by_label("Discard unsaved changes and close?");
    assert!(settings(&mut screen).modal_tab().is_some());
    screen.get_by_label("Keep editing").click();
    screen.run();
    assert!(
        screen
            .query_by_label("Discard unsaved changes and close?")
            .is_none()
    );
    screen.get_by_label("● Unsaved changes: slow");

    screen.key_press(egui::Key::Escape);
    screen.run();
    screen.get_by_label("Discard unsaved changes and close?");
    screen.get_by_label("Discard and close").click();
    screen.run();
    assert!(settings(&mut screen).modal_tab().is_none());
    assert!(asked(&mut screen).is_empty());
}

// Given: a screen that has not read the settings yet
// When: Model Settings is opened and the screen is drawn over several frames
// Then: the snapshot is asked for once (by maintain), not once per frame and not once per
//       surface that draws the feature
#[test]
fn the_snapshot_is_asked_for_by_maintain_once_while_the_modal_is_open() {
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    settings(&mut screen).open_modal(SettingsTab::Providers);
    screen.run_steps(6);
    let snapshots = asked(&mut screen)
        .iter()
        .filter(|action| {
            matches!(
                query_of(action),
                Query::Settings(SettingsQuery::GetSettingsSnapshot {})
            )
        })
        .count();
    assert_eq!(snapshots, 1);
    screen.run_steps(6);
    assert!(
        asked(&mut screen).is_empty(),
        "nothing is asked again while it waits"
    );
}

fn disabled(screen: &Harness<'static, Screen>, label: &str) -> bool {
    screen.get_by_label(label).accesskit_node().is_disabled()
}

// Given: unsaved edits in two roles (Orchestrator, then Explorer)
// When: the Explorer is saved and its end seen
// Then: only the Explorer is written, the Orchestrator is still unsaved and says so in the
//       footer, the profile control stays locked until it is discarded, and Discard of the
//       selected role leaves nothing else changed
#[test]
fn saving_one_role_leaves_the_other_edit_and_the_profile_waits_for_it() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Agent roles");
    edit_selected_role(&mut screen);
    pick_role(&mut screen, "Explorer");
    edit_selected_role(&mut screen);
    screen.get_by_label("● Unsaved changes: Explorer (+1 other role)");
    assert!(disabled(&screen, "Profile menu"));

    click(&mut screen, "Save role");
    let explorer = document_save(&mut screen);
    let saved = SettingsDocument::parse(&explorer.text)
        .unwrap()
        .seed_roles(None);
    assert_eq!(saved.agents.explorer.generation.temperature, Some(0.0));
    assert_eq!(saved.agents.orchestrator.generation.temperature, None);
    end(&mut screen, &explorer, OperationState::Ready);
    answer_snapshot(&mut screen, 'e', &explorer.text);
    screen.get_by_label("● 1 other role unsaved");
    assert!(disabled(&screen, "Profile menu"));

    pick_role(&mut screen, "Orchestrator");
    click(&mut screen, "Discard");
    assert!(screen.query_by_label_contains("unsaved").is_none());
    assert!(!disabled(&screen, "Profile menu"));
    assert!(asked(&mut screen).is_empty());
}

// Given: the Agent roles of a file where only the Orchestrator and Explorer have routes
// When: the Compactor, the Image generator and the Planner are picked
// Then: the Compactor and the Image generator say what an empty chain means and are not
//       reported as having no route; the Planner is, and offers to create its route
#[test]
fn an_inheriting_compactor_and_an_image_generator_without_a_chain_are_not_errors() {
    let mut screen = loaded_with(DOCUMENT, None);
    open_modal(&mut screen, "Agent roles");
    pick_role(&mut screen, "Compactor");
    screen.get_by_label_contains("Uses the running role's chain");
    assert!(
        screen
            .query_by_label_contains("Undefined (no route)")
            .is_none()
    );
    pick_role(&mut screen, "Image generator");
    screen.get_by_label_contains("Disabled (generate_image unavailable)");
    assert!(
        screen
            .query_by_label_contains("Undefined (no route)")
            .is_none()
    );
    assert!(screen.query_by_label("Create route").is_none());
    pick_role(&mut screen, "Planner");
    screen.get_by_label_contains("Undefined (no route)");
    screen.get_by_label("Create route");
}

// Given: Routing and Agent Roles at the smallest window the app supports
// When: a route is added (Routing) and a role is edited (Agent Roles)
// Then: the footer's Discard and Save, the pinned bar and the open card (or the table and
//       the pane) are all inside the window
#[test]
fn routing_and_roles_fit_the_smallest_window() {
    let mut screen = loaded_with(DOCUMENT, None);
    screen.set_size(egui::vec2(960.0, 600.0));
    let window = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(960.0, 600.0));
    open_modal(&mut screen, "Routing");
    click(&mut screen, "Add route");
    for label in ["Add route", "Save routing", "Discard", "Logical model name"] {
        let rect = screen.get_by_label(label).rect();
        assert!(window.contains_rect(rect), "{label} at {rect:?}");
    }
    screen.get_by_label("Discard").click();
    screen.run();
    open_modal(&mut screen, "Agent roles");
    edit_selected_role(&mut screen);
    for label in ["Save role", "Discard", "Role route", "Temperature"] {
        let rect = screen.get_by_label(label).rect();
        assert!(window.contains_rect(rect), "{label} at {rect:?}");
    }
    for role in ["Orchestrator", "Explorer"] {
        let rect = screen.get_by_role_and_label(Role::Button, role).rect();
        assert!(window.contains_rect(rect), "{role} at {rect:?}");
    }
}

#[cfg(test)]
#[path = "modal_evidence.rs"]
mod evidence;
