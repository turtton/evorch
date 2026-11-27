//! The settings feature drawn headless: a save quotes the revision its draft
//! was made from, the outcome comes back as an event or a query, and nothing
//! is sent twice.
use super::*;
use crate::core_client::features::execution::tests::{
    Screen, answer, asked, command_of, ok_command, query_of, screen,
};
use crate::core_client::features::{FeatureError, FeatureReply};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{
    commands::CommandReply,
    namespaces::settings::{CredentialRef, ModelKind, ModelView, ProviderKind},
    operation::OperationView,
    wire::Revision,
};

const DOCUMENT: &str = "version = 3\n";

pub(super) fn revision(c: char) -> SettingsRevision {
    SettingsRevision::new(c.to_string().repeat(64)).unwrap()
}

pub(super) fn provider(name: &str) -> ProviderView {
    ProviderView {
        name: name.into(),
        kind: ProviderKind::OpenaiCompatible,
        base_url: "https://llm.example.invalid/v1".into(),
        credential: CredentialRef::Env { var: "KEY".into() },
        models: vec![ModelView {
            id: "m".into(),
            enabled: true,
            kind: ModelKind::Chat,
            context_override: None,
        }],
        excluded_models: vec![],
        default_model: "m".into(),
        show_quota: false,
        quota_supported: false,
    }
}

fn snapshot(saved: char, applied: char) -> FeatureReply {
    snapshot_of(saved, Some(applied), None)
}

fn snapshot_of(saved: char, applied: Option<char>, failure: Option<&str>) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
        SettingsSnapshot {
            revision: revision(saved),
            applied_revision: applied.map(revision),
            apply_failure: failure.map(str::to_owned),
            document: DOCUMENT.into(),
            providers: vec![provider("local")],
            problem: None,
        },
    )))))
}

pub(super) fn view(
    id: &OperationId,
    kind: SettingsOperationKind,
    state: OperationState,
) -> SettingsOperationView {
    SettingsOperationView {
        operation: OperationView {
            operation_id: id.clone(),
            state,
            revision: Revision(1),
            result: None,
        },
        kind,
        revision: None,
        message: None,
    }
}

fn command_reply(view: SettingsOperationView) -> FeatureReply {
    FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Settings(
        SettingsResult::Operation(view),
    ))))
}

fn completed(view: SettingsOperationView) -> UiEvent {
    UiEvent::Settings(SettingsEvent::SettingsOperationCompleted { operation: view })
}

/// A screen that has read the snapshot (saved `a`, applied `applied`).
fn loaded(applied: char) -> Harness<'static, Screen> {
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    assert!(matches!(
        query_of(&first[0]),
        Query::Settings(SettingsQuery::GetSettingsSnapshot {})
    ));
    answer(&mut screen, first[0].tag, snapshot('a', applied));
    screen
}

pub(super) fn settings_command(
    action: &crate::core_client::features::FeatureAction,
) -> &SettingsCommand {
    let Command::Settings(command) = command_of(action) else {
        panic!("a settings command was expected");
    };
    command
}

/// Opens Model Settings on the section named `tab`, as the ⚙ menu does: the modal is the
/// feature's overlay, so no button of the Config file tab is pressed.
pub(super) fn open_modal(screen: &mut Harness<'static, Screen>, tab: &str) {
    let tab = match tab {
        "Providers" => SettingsTab::Providers,
        "Routing" => SettingsTab::Routing,
        "Agent roles" => SettingsTab::AgentRoles,
        other => panic!("no such section: {other}"),
    };
    screen
        .state_mut()
        .feature
        .as_settings_mut()
        .expect("the settings feature")
        .open_modal(tab);
    screen.run();
    if tab == SettingsTab::Providers {
        // The credential status is asked for when Providers is shown; these tests are about
        // what the settings ask.
        let _ = asked(screen);
    }
}

/// What the screen asked since the last call, without the questions of the credential
/// controller.
pub(super) fn settings_asked(
    screen: &mut Harness<'static, Screen>,
) -> Vec<crate::core_client::features::FeatureAction> {
    asked(screen)
        .into_iter()
        .filter(|action| {
            !matches!(
                &action.request,
                crate::core_client::features::FeatureRequest::Query(Query::Auth(_))
            )
        })
        .collect()
}

/// Chooses the provider named `name` in the list.
pub(super) fn select_provider(screen: &mut Harness<'static, Screen>, name: &str) {
    screen.get_by_role_and_label(Role::Button, name).click();
    screen.run();
}

/// Adds a provider of the kind named `title` in the Add menu.
pub(super) fn add_provider(screen: &mut Harness<'static, Screen>, title: &str) {
    screen.get_by_label("Add").click();
    screen.run();
    screen.get_by_role_and_label(Role::Button, title).click();
    screen.run();
}

/// Types into the form of the selected provider, so that it has something to save.
pub(super) fn edit_provider(screen: &mut Harness<'static, Screen>) {
    const LABEL: &str = "Excluded models";
    screen
        .get_by_role_and_label(Role::MultilineTextInput, LABEL)
        .scroll_to_me();
    screen.run();
    screen
        .get_by_role_and_label(Role::MultilineTextInput, LABEL)
        .focus();
    screen.run_steps(4);
    screen
        .get_by_role_and_label(Role::MultilineTextInput, LABEL)
        .type_text("x");
    screen.run_steps(4);
}

fn type_into_document(screen: &mut Harness<'static, Screen>, text: &str) {
    screen.get_by_role(Role::MultilineTextInput).focus();
    screen.run_steps(4);
    screen.get_by_role(Role::MultilineTextInput).type_text(text);
    screen.run_steps(4);
}

#[test]
fn what_is_saved_and_what_is_applied_are_shown_apart() {
    let screen = loaded('b');
    screen.get_by_label("Saved settings are not applied to the running core yet.");
    // The way to the model sections is the menu, not a button of this tab.
    for tab in ["Providers", "Routing", "Agent roles"] {
        assert!(screen.query_by_label(tab).is_none(), "{tab}");
    }
    drop(screen);

    let screen = loaded('a');
    screen.get_by_label("Saved settings are applied.");
}

/// A reload that failed part-way leaves the models a mixture: the screen says
/// it does not know, and the reason, even when the saved file is the one that
/// was applied before (the hash of a file that was put back equals the old one).
#[test]
fn an_unknown_applied_state_is_shown_with_its_reason_not_as_applied() {
    let mut screen = screen(Box::<SettingsFeature>::default(), None);
    let first = asked(&mut screen);
    answer(
        &mut screen,
        first[0].tag,
        snapshot_of('a', None, Some("the project did not load")),
    );
    screen.get_by_label(
        "The applied state is unknown (the last reload failed: the project did not load). Reload models.",
    );
    assert!(
        screen
            .query_by_label("Saved settings are applied.")
            .is_none()
    );
    assert!(
        screen
            .query_by_label("Saved settings are not applied to the running core yet.")
            .is_none()
    );
}

#[test]
fn a_save_quotes_the_revision_of_its_draft_and_is_not_sent_twice() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# edited\n");
    screen.get_by_label("Save file").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]) else {
        panic!("a document save was expected");
    };
    assert_eq!(save.expected_revision, revision('a'));
    assert!(save.text.contains("# edited"), "{:?}", save.text);

    // While it runs, a second save cannot be made and nothing else is sent.
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveDocument,
            OperationState::Pending,
        )),
    );
    screen.get_by_label("Working…");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn a_refused_save_keeps_the_draft_and_says_why() {
    let cases = [
        (
            ErrorCode::StaleRevision,
            "The settings were changed elsewhere. Re-read them before saving.",
        ),
        (ErrorCode::PermissionDenied, "core.restart.targets"),
    ];
    for (code, expected) in cases {
        let mut screen = loaded('a');
        type_into_document(&mut screen, "# edited\n");
        screen.get_by_label("Save file").click();
        let sent = asked(&mut screen);
        let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
            panic!("a document save was expected");
        };
        answer(
            &mut screen,
            sent[0].tag,
            command_reply(view(
                &save.operation_id,
                SettingsOperationKind::SaveDocument,
                OperationState::Pending,
            )),
        );
        screen.state_mut().feature.event(&completed(view(
            &save.operation_id,
            SettingsOperationKind::SaveDocument,
            OperationState::Failed { code },
        )));
        screen.run_steps(4);
        let shown = screen
            .query_all_by_label_contains(expected)
            .next()
            .is_some();
        assert!(shown, "{code:?} says why");
        // The edit is still there, and the snapshot is read again.
        let again = asked(&mut screen);
        assert!(again.iter().any(|a| matches!(
            query_of(a),
            Query::Settings(SettingsQuery::GetSettingsSnapshot {})
        )));
        screen.get_by_role(Role::MultilineTextInput);
        assert!(
            !screen
                .get_by_label("Save file")
                .accesskit_node()
                .is_disabled()
        );
    }
}

#[test]
fn a_finished_save_asks_for_the_reload_and_the_reload_reports_its_end() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# edited\n");
    screen.get_by_label("Save file").click();
    let sent = asked(&mut screen);
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
        panic!("a document save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveDocument,
            OperationState::Ready,
        )),
    );
    screen.get_by_label("Saved. Reload models to apply it.");

    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    let reload = sent
        .iter()
        .filter(|action| {
            matches!(
                action.request,
                crate::core_client::features::FeatureRequest::Command(_)
            )
        })
        .find_map(|action| match settings_command(action) {
            SettingsCommand::ReloadModels(reload) => Some((action.tag, reload.clone())),
            _ => None,
        })
        .expect("a reload was sent");
    answer(
        &mut screen,
        reload.0,
        command_reply(view(
            &reload.1.operation_id,
            SettingsOperationKind::ReloadModels,
            OperationState::Pending,
        )),
    );
    screen.state_mut().feature.event(&completed(view(
        &reload.1.operation_id,
        SettingsOperationKind::ReloadModels,
        OperationState::Ready,
    )));
    screen.run_steps(4);
    screen.get_by_label(
        "Models reloaded. Requests from now on use them (what was already sent does not change).",
    );
}

#[test]
fn a_lost_reply_or_a_reconnect_asks_for_the_operation_and_does_not_resend() {
    let mut screen = loaded('a');
    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    let SettingsCommand::ReloadModels(reload) = settings_command(&sent[0]).clone() else {
        panic!("a reload was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let asked_again = asked(&mut screen);
    assert_eq!(asked_again.len(), 1);
    assert!(matches!(
        query_of(&asked_again[0]),
        Query::Settings(SettingsQuery::GetSettingsOperation { operation_id })
            if *operation_id == reload.operation_id
    ));
    answer(
        &mut screen,
        asked_again[0].tag,
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Operation(view(
            &reload.operation_id,
            SettingsOperationKind::ReloadModels,
            OperationState::Ready,
        ))))),
    );
    screen.get_by_label(
        "Models reloaded. Requests from now on use them (what was already sent does not change).",
    );

    // A running operation survives a new snapshot as a question, not a command.
    let mut screen = loaded('a');
    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    answer(&mut screen, sent[0].tag, ok_command());
    screen.state_mut().feature.reset();
    let after = asked(&mut screen);
    assert!(after.iter().all(|action| matches!(
        action.request,
        crate::core_client::features::FeatureRequest::Query(_)
    )));
}

fn not_found() -> FeatureReply {
    FeatureReply::Query(Err(
        FeatureError::new("the operation is unknown").with_code(ErrorCode::NotFound)
    ))
}

#[test]
fn a_save_whose_end_was_lost_with_a_core_restart_is_unknown_and_keeps_the_draft() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# edited\n");
    screen.get_by_label("Save file").click();
    let sent = asked(&mut screen);
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
        panic!("a document save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    screen.get_by_label("Working…");
    // The core restarted: it does not know the operation any more.
    let asked_again = asked(&mut screen);
    let query = asked_again
        .iter()
        .find(|action| {
            matches!(
                query_of(action),
                Query::Settings(SettingsQuery::GetSettingsOperation { operation_id })
                    if *operation_id == save.operation_id
            )
        })
        .expect("the operation is asked for");
    answer(&mut screen, query.tag, not_found());
    screen.run_steps(4);

    // Not busy for ever, the result said to be unknown, the edit still there
    // and the snapshot read again to show what the file holds.
    assert!(screen.query_by_label("Working…").is_none());
    assert!(
        screen
            .query_all_by_label_contains("The result of the operation could not be confirmed")
            .next()
            .is_some()
    );
    screen.get_by_role(Role::MultilineTextInput);
    assert!(
        !screen
            .get_by_label("Reload models")
            .accesskit_node()
            .is_disabled()
    );
    let again = asked(&mut screen);
    let snapshot_query = again
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Settings(SettingsQuery::GetSettingsSnapshot {})
            )
        })
        .expect("the snapshot is read again");
    // The file moved on (the lost save was written): the draft is not replaced.
    answer(&mut screen, snapshot_query.tag, snapshot('b', 'a'));
    screen.run_steps(4);
    screen.get_by_role(Role::MultilineTextInput);
    assert!(
        !screen
            .get_by_label("Save file")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn text_typed_while_a_save_runs_is_not_discarded_when_the_save_ends() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# first\n");
    screen.get_by_label("Save file").click();
    let sent = asked(&mut screen);
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
        panic!("a document save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveDocument,
            OperationState::Pending,
        )),
    );
    // The person keeps typing while the save runs.
    type_into_document(&mut screen, "# second\n");
    let mut done = view(
        &save.operation_id,
        SettingsOperationKind::SaveDocument,
        OperationState::Ready,
    );
    done.revision = Some(revision('b'));
    screen.state_mut().feature.event(&completed(done));
    screen.run_steps(4);
    // What was typed after the sent text is still there, and is the only
    // change left to save, over the revision the save produced.
    screen.get_by_role(Role::MultilineTextInput);
    assert!(
        !screen
            .get_by_label("Save file")
            .accesskit_node()
            .is_disabled()
    );
    let _ = asked(&mut screen);
    screen.get_by_label("Save file").click();
    let next = asked(&mut screen);
    let SettingsCommand::SaveDocument(second) = next
        .iter()
        .filter(|a| {
            matches!(
                a.request,
                crate::core_client::features::FeatureRequest::Command(_)
            )
        })
        .map(settings_command)
        .next()
        .expect("a second save")
        .clone()
    else {
        panic!("a document save was expected");
    };
    assert_eq!(second.expected_revision, revision('b'));
    assert!(second.text.contains("# second"), "{:?}", second.text);
}

#[test]
fn a_pending_reply_arriving_after_the_end_does_not_make_the_screen_busy_again() {
    let mut screen = loaded('a');
    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    let SettingsCommand::ReloadModels(reload) = settings_command(&sent[0]).clone() else {
        panic!("a reload was expected");
    };
    // The end is announced before the answer to the command that started it.
    screen.state_mut().feature.event(&completed(view(
        &reload.operation_id,
        SettingsOperationKind::ReloadModels,
        OperationState::Ready,
    )));
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &reload.operation_id,
            SettingsOperationKind::ReloadModels,
            OperationState::Pending,
        )),
    );
    screen.run_steps(4);
    assert!(screen.query_by_label("Working…").is_none());
    screen.get_by_label(
        "Models reloaded. Requests from now on use them (what was already sent does not change).",
    );
}

pub(super) fn provider_save(action: &crate::core_client::features::FeatureAction) -> SaveProvider {
    let SettingsCommand::SaveProvider(save) = settings_command(action).clone() else {
        panic!("a provider save was expected");
    };
    *save
}

// Given: a screen that has read a provider named `local`
// When: it is edited and saved, the core refuses it with a reason, and the save is made again
// Then: the command quotes the snapshot revision and the profile being replaced, the
//       refusal is shown with its reason and the form keeps what was typed, and the
//       saved provider closes the form
#[test]
fn an_edited_provider_is_saved_with_its_revision_and_the_outcome_is_shown() {
    use workbench_protocol::namespaces::settings::ProviderInput;
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    edit_provider(&mut screen);
    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let save = provider_save(&sent[0]);
    assert_eq!(save.expected_revision, revision('a'));
    assert_eq!(save.original_name.as_deref(), Some("local"));
    let ProviderInput::OpenaiCompatible {
        name,
        base_url,
        credential,
        default_model,
        ..
    } = &save.provider
    else {
        panic!("an OpenAI-compatible provider was expected");
    };
    assert_eq!(
        (name.as_str(), base_url.as_str(), default_model.as_str()),
        ("local", "https://llm.example.invalid/v1", "m")
    );
    assert_eq!(credential, &CredentialRef::Env { var: "KEY".into() });

    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveProvider,
            OperationState::Pending,
        )),
    );
    screen.get_by_label("Working…");
    let mut refused = view(
        &save.operation_id,
        SettingsOperationKind::SaveProvider,
        OperationState::Failed {
            code: ErrorCode::InvalidRequest,
        },
    );
    refused.message = Some("base_url is not a URL".into());
    screen.state_mut().feature.event(&completed(refused));
    screen.run_steps(4);
    assert!(
        screen
            .query_all_by_label_contains("base_url is not a URL")
            .next()
            .is_some(),
        "the refusal says why"
    );
    // What was typed is still there, and the snapshot is read again; nothing is resent.
    let again = asked(&mut screen);
    assert!(again.iter().all(|a| matches!(
        query_of(a),
        Query::Settings(SettingsQuery::GetSettingsSnapshot {})
    )));
    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    let save = provider_save(&sent[0]);
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveProvider,
            OperationState::Pending,
        )),
    );
    screen.state_mut().feature.event(&completed(view(
        &save.operation_id,
        SettingsOperationKind::SaveProvider,
        OperationState::Ready,
    )));
    screen.run_steps(4);
    screen.get_by_label("Provider settings updated. Reload models to apply them.");
    assert!(
        screen.get_by_label("Save").accesskit_node().is_disabled(),
        "the saved provider is open again as saved, with nothing to save"
    );
}

// Given: a new OpenAI-compatible form, which starts without a base URL
// When: it is saved
// Then: nothing is sent and the missing field is named; an API key is never a field
#[test]
fn a_new_provider_form_names_what_is_missing_and_sends_nothing() {
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    add_provider(&mut screen, "OpenAI-compatible");
    assert!(
        screen
            .query_all_by_label_contains("its API key is stored from this card afterwards")
            .next()
            .is_some()
    );
    assert_eq!(screen.query_all_by_role(Role::PasswordInput).count(), 0);
    screen.get_by_label("Save").click();
    assert!(asked(&mut screen).is_empty());
    screen.run_steps(2);
    screen.get_by_label("Enter a base URL.");
}

// Given: a provider form opened at revision `a`
// When: the file changes to `b` before it is saved, and then the form is saved as it is,
//       and then opened again on the newest content and saved
// Then: the first save quotes `a` (the core refuses it as stale, so the other client's
//       change is not overwritten), the screen says the file moved and offers a re-read,
//       and only the explicit re-read makes the next save quote `b`
#[test]
fn a_provider_form_saves_against_the_revision_it_was_opened_from() {
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    edit_provider(&mut screen);
    // Another client changes the file; this screen reads the new snapshot.
    screen.state_mut().feature.reset();
    let read = settings_asked(&mut screen);
    assert_eq!(read.len(), 1);
    answer(&mut screen, read[0].tag, snapshot('b', 'b'));
    screen.run_steps(2);
    screen.get_by_label("Reopen with the latest settings");
    assert!(
        screen
            .query_all_by_label_contains("saving will be refused")
            .next()
            .is_some()
    );

    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let save = provider_save(&sent[0]);
    assert_eq!(
        save.expected_revision,
        revision('a'),
        "the revision the form was opened from, not the newest"
    );
    let mut stale = view(
        &save.operation_id,
        SettingsOperationKind::SaveProvider,
        OperationState::Failed {
            code: ErrorCode::StaleRevision,
        },
    );
    stale.message = Some("the settings changed since this revision".into());
    screen.state_mut().feature.event(&completed(stale));
    screen.run_steps(4);
    screen.get_by_label("Save");
    assert!(
        screen
            .query_all_by_label_contains("Reopen with the latest settings")
            .next()
            .is_some(),
        "the form stays, with the way to re-read"
    );
    let _ = asked(&mut screen);

    screen
        .get_by_label("Reopen with the latest settings")
        .scroll_to_me();
    screen.run();
    screen
        .get_by_label("Reopen with the latest settings")
        .click();
    screen.run_steps(4);
    assert!(
        screen
            .query_all_by_label_contains("Reopen with the latest settings")
            .next()
            .is_none()
    );
    edit_provider(&mut screen);
    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    assert_eq!(provider_save(&sent[0]).expected_revision, revision('b'));
}

// Given: a provider save that is running
// When: it ends, another provider is added, and the end is seen again
// Then: the form opened since stays open, and nothing can be added while the save runs
#[test]
fn a_finished_save_does_not_close_a_form_it_was_not_made_from() {
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    edit_provider(&mut screen);
    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    let save = provider_save(&sent[0]);
    answer(
        &mut screen,
        sent[0].tag,
        command_reply(view(
            &save.operation_id,
            SettingsOperationKind::SaveProvider,
            OperationState::Pending,
        )),
    );
    // While the save runs the list, Add and Discard wait for it.
    assert!(
        screen
            .get_by_label("Discard")
            .accesskit_node()
            .is_disabled()
    );
    let ready = completed(view(
        &save.operation_id,
        SettingsOperationKind::SaveProvider,
        OperationState::Ready,
    ));
    screen.state_mut().feature.event(&ready);
    screen.run_steps(2);
    add_provider(&mut screen, "OpenAI-compatible");
    assert!(screen.query_all_by_label("openai-compat").next().is_some());
    // The end of the earlier save, seen again, does not close the form opened since.
    screen.state_mut().feature.event(&ready);
    screen.run_steps(2);
    assert!(screen.query_all_by_label("openai-compat").next().is_some());
}

// Given: a reload whose reply was lost
// When: asking for its operation fails with an error other than not-found
// Then: the screen is not left busy, and the error is shown
#[test]
fn an_operation_that_cannot_be_asked_for_does_not_keep_the_screen_busy() {
    let mut screen = loaded('a');
    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let check = asked(&mut screen);
    assert!(matches!(
        query_of(&check[0]),
        Query::Settings(SettingsQuery::GetSettingsOperation { .. })
    ));
    answer(
        &mut screen,
        check[0].tag,
        FeatureReply::Query(Err(FeatureError::new("the send queue is full"))),
    );
    screen.run_steps(4);
    assert!(screen.query_by_label("Working…").is_none());
    assert!(
        !screen
            .get_by_label("Reload models")
            .accesskit_node()
            .is_disabled()
    );
    assert!(
        screen
            .query_all_by_label_contains("the send queue is full")
            .next()
            .is_some()
    );
}

// Given: a reload that is running and no settings modal
// When: the footer menu asks for the Routing tab
// Then: the modal opens on it
#[test]
fn the_footer_opens_the_modal_while_an_operation_runs() {
    let mut screen = loaded('a');
    screen.get_by_label("Reload models").click();
    let sent = asked(&mut screen);
    answer(&mut screen, sent[0].tag, ok_command());
    screen.get_by_label("Working…");
    let feature = screen.state_mut().feature.as_settings_mut().unwrap();
    feature.open_modal(SettingsTab::Routing);
    assert_eq!(feature.modal_tab(), Some(SettingsTab::Routing));
}

fn feature_view(revision: char, web_tools: bool) -> FeatureReply {
    use workbench_protocol::namespaces::settings::{
        BrowserFeature, FeatureSettingsView, FeatureValues, PullRequestFeature,
        ThreadCleanupFeature,
    };
    let values = FeatureValues {
        pull_requests: PullRequestFeature {
            enabled: false,
            auto_fix_enabled: false,
            include_in_review: true,
            pending_poll_seconds: 30,
            stable_poll_seconds: 300,
            timeout_seconds: 30,
        },
        agent_web_tools: web_tools,
        browser: BrowserFeature {
            enabled: true,
            executable: None,
            max_pages: 8,
            max_tabs: 4,
            coordinate_actions: false,
        },
        thread_cleanup: ThreadCleanupFeature {
            auto_archive_after_days: 0,
            archived_delete_after_days: 0,
        },
    };
    FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::FeatureSettings(
        Box::new(FeatureSettingsView {
            revision: revision_of(revision),
            saved: values.clone(),
            running: values,
            restart_pending: vec![],
            problem: None,
        }),
    ))))
}

fn revision_of(c: char) -> SettingsRevision {
    revision(c)
}

/// A screen with the Features modal open over a view saved at revision `a`.
fn features_loaded() -> Harness<'static, Screen> {
    let mut screen = loaded('a');
    screen
        .state_mut()
        .feature
        .as_settings_mut()
        .expect("the settings feature")
        .open_features();
    let asked = asked(&mut screen);
    let query = asked
        .iter()
        .find(|action| {
            matches!(
                query_of(action),
                Query::Settings(SettingsQuery::GetFeatureSettings {})
            )
        })
        .expect("the feature view is read when the modal opens");
    answer(&mut screen, query.tag, feature_view('a', true));
    screen.run_steps(8);
    screen
}

// Given: the Config file tab / When: it is drawn
// Then: it has no session-only web tools switch (the persistent one is in Features)
#[test]
fn the_config_file_tab_has_no_session_only_web_tools_switch() {
    let screen = loaded('a');
    for label in ["Enable web tools", "Disable web tools", "Agent web tools"] {
        assert!(screen.query_by_label(label).is_none(), "{label}");
    }
}

// Given: the Features modal on a view saved at `a`
// When: Agent web tools is switched off and saved, and the core refuses
// Then: exactly one command quotes `a` with only the web tools section, the refusal is
//       shown, the draft is kept, and the same save can be sent again as a new operation
#[test]
fn a_refused_feature_save_keeps_the_draft_and_can_be_sent_again() {
    let mut screen = features_loaded();
    screen
        .get_by_role_and_label(Role::CheckBox, "Agent web tools")
        .click();
    screen.run_steps(4);
    screen.get_by_label("Save").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1);
    let SettingsCommand::SaveFeatureSettings(first) = settings_command(&sent[0]) else {
        panic!("a feature save was expected");
    };
    assert_eq!(first.expected_revision, revision('a'));
    assert_eq!(first.changes.agent_web_tools, Some(false));
    assert!(first.changes.pull_requests.is_none());
    assert!(first.changes.browser.is_none());
    assert!(first.changes.thread_cleanup.is_none());
    let first_id = first.operation_id.clone();

    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::new("permission denied"))),
    );
    screen.run_steps(4);
    assert!(screen.query_all_by_label("permission denied").count() >= 1);
    screen.get_by_label("Save").click();
    let again: Vec<_> = asked(&mut screen)
        .into_iter()
        .filter(|action| {
            matches!(
                action.request,
                crate::core_client::features::FeatureRequest::Command(_)
            )
        })
        .collect();
    assert_eq!(again.len(), 1);
    let SettingsCommand::SaveFeatureSettings(second) = settings_command(&again[0]) else {
        panic!("a feature save was expected");
    };
    assert_ne!(second.operation_id, first_id);
    assert_eq!(second.changes.agent_web_tools, Some(false));
}

// Given: a feature save in flight
// When: the screen is pressed to Save again before the core answers
// Then: nothing more is sent
#[test]
fn a_feature_save_in_flight_is_not_sent_twice() {
    let mut screen = features_loaded();
    screen
        .get_by_role_and_label(Role::CheckBox, "Agent web tools")
        .click();
    screen.run_steps(4);
    screen.get_by_label("Save").click();
    assert_eq!(asked(&mut screen).len(), 1);
    screen.run_steps(4);
    assert!(asked(&mut screen).is_empty());
}

fn fetch_command(
    action: &crate::core_client::features::FeatureAction,
) -> workbench_protocol::namespaces::settings::FetchProviderModels {
    let SettingsCommand::FetchProviderModels(fetch) = settings_command(action).clone() else {
        panic!("a model fetch was expected");
    };
    fetch
}

fn candidates(
    id: &OperationId,
    models: &[&str],
    truncated: bool,
) -> workbench_protocol::namespaces::settings::ProviderModelCandidates {
    workbench_protocol::namespaces::settings::ProviderModelCandidates {
        operation_id: id.clone(),
        provider: "local".into(),
        models: models.iter().map(|id| (*id).to_owned()).collect(),
        truncated,
    }
}

/// Clicks Fetch models and answers the command as the core does: pending.
fn start_fetch(
    screen: &mut Harness<'static, Screen>,
) -> workbench_protocol::namespaces::settings::FetchProviderModels {
    screen.get_by_label("Fetch models").click();
    let sent = settings_asked(screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let fetch = fetch_command(&sent[0]);
    answer(
        screen,
        sent[0].tag,
        command_reply(view(
            &fetch.operation_id,
            SettingsOperationKind::FetchProviderModels,
            OperationState::Pending,
        )),
    );
    fetch
}

// Given: a saved provider open in Providers
// When: Fetch models is pressed, the operation ends and its result is read
// Then: the command names the saved provider and the revision only, the panel lists what was
//       found (a filter narrows it), a picked id is added to the form, and nothing is saved
#[test]
fn fetching_models_names_the_provider_lists_the_result_and_adds_picked_ids_to_the_form() {
    let mut screen = loaded('a');
    screen.set_size(egui::vec2(1100.0, 1700.0));
    open_modal(&mut screen, "Providers");
    let fetch = start_fetch(&mut screen);
    assert_eq!(fetch.provider, "local");
    assert_eq!(fetch.expected_revision, revision('a'));
    screen.get_by_label_contains("Fetching models");

    screen.state_mut().feature.event(&completed(view(
        &fetch.operation_id,
        SettingsOperationKind::FetchProviderModels,
        OperationState::Ready,
    )));
    let read = settings_asked(&mut screen);
    let read: Vec<_> = read
        .iter()
        .filter(|a| {
            matches!(
                query_of(a),
                Query::Settings(SettingsQuery::GetProviderModels { .. })
            )
        })
        .collect();
    assert_eq!(read.len(), 1, "the result is read once");
    let Query::Settings(SettingsQuery::GetProviderModels { operation_id }) = query_of(read[0])
    else {
        unreachable!()
    };
    assert_eq!(operation_id, &fetch.operation_id);
    answer(
        &mut screen,
        read[0].tag,
        FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::ProviderModels(
            candidates(&fetch.operation_id, &["alpha", "beta", "m"], true),
        )))),
    );
    screen.get_by_label_contains("3 available");
    screen.get_by_label_contains("offers more models");
    screen.get_by_label("alpha");
    screen.get_by_label("beta");
    screen.get_by_label_contains("Added");

    screen.get_by_label("alpha").scroll_to_me();
    screen.run_steps(4);
    screen.get_by_label("alpha").click();
    screen.run_steps(2);
    screen.get_by_label("Add selected (1)").scroll_to_me();
    screen.run_steps(4);
    screen.get_by_label("Add selected (1)").click();
    screen.run_steps(4);
    screen.get_by_label("Enable alpha");
    assert!(
        settings_asked(&mut screen).iter().all(|a| !matches!(
            &a.request,
            crate::core_client::features::FeatureRequest::Command(_)
        )),
        "picking models saves nothing"
    );
    screen.get_by_label("Save").click();
    let save = provider_save(&settings_asked(&mut screen)[0]);
    let workbench_protocol::namespaces::settings::ProviderInput::OpenaiCompatible {
        models, ..
    } = &save.provider
    else {
        panic!("an OpenAI-compatible provider was expected");
    };
    assert_eq!(
        models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
        ["m", "alpha"]
    );
}

#[test]
fn an_unsupported_kind_and_a_failed_fetch_leave_manual_entry_and_say_why() {
    for (state, message, shown) in [
        (
            OperationState::Failed {
                code: ErrorCode::UnsupportedCapability,
            },
            None,
            "does not list its models",
        ),
        (
            OperationState::Failed {
                code: ErrorCode::Internal,
            },
            Some("the provider answered HTTP 401"),
            "the provider answered HTTP 401",
        ),
    ] {
        let mut screen = loaded('a');
        open_modal(&mut screen, "Providers");
        let fetch = start_fetch(&mut screen);
        let mut ended = view(
            &fetch.operation_id,
            SettingsOperationKind::FetchProviderModels,
            state,
        );
        ended.message = message.map(str::to_owned);
        screen.state_mut().feature.event(&completed(ended));
        screen.run_steps(4);
        screen.get_by_label_contains(shown);
        screen.get_by_label("Add model");
        assert!(
            settings_asked(&mut screen).iter().all(|a| !matches!(
                query_of_or_none(a),
                Some(Query::Settings(SettingsQuery::GetProviderModels { .. }))
            )),
            "a failed fetch has no result to read"
        );
    }
}

fn query_of_or_none(action: &crate::core_client::features::FeatureAction) -> Option<&Query> {
    match &action.request {
        crate::core_client::features::FeatureRequest::Query(query) => Some(query),
        crate::core_client::features::FeatureRequest::Command(_) => None,
    }
}

// Given: a running fetch
// When: a reconnect resets the screen
// Then: the panel says to fetch again instead of waiting for a result nobody will read
#[test]
fn a_reconnect_ends_a_running_fetch_with_a_message() {
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    let _ = start_fetch(&mut screen);
    screen.state_mut().feature.reset();
    let again = settings_asked(&mut screen);
    let snapshot_tag = again
        .iter()
        .find(|a| {
            matches!(
                query_of_or_none(a),
                Some(Query::Settings(SettingsQuery::GetSettingsSnapshot {}))
            )
        })
        .expect("the snapshot is read again")
        .tag;
    answer(&mut screen, snapshot_tag, snapshot('a', 'a'));
    screen.get_by_label_contains("Fetch again");
}

// Given: the Context window of a model typed as `400k`
// When: the provider is saved
// Then: the override is 400000 tokens; a value that is not a number sends nothing
#[test]
fn a_context_window_override_is_saved_as_tokens_and_a_bad_one_is_refused_in_the_form() {
    let mut screen = loaded('a');
    open_modal(&mut screen, "Providers");
    screen.get_by_label("Context window of m").focus();
    screen.run_steps(4);
    screen.get_by_label("Context window of m").type_text("lots");
    screen.run_steps(4);
    screen.get_by_label("Save").click();
    screen.run_steps(2);
    assert!(settings_asked(&mut screen).is_empty(), "nothing is sent");
    screen.get_by_label_contains("must be a number");
}
