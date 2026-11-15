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
    namespaces::settings::{CredentialRef, ModelView, ProviderKind},
    operation::OperationView,
    wire::Revision,
};

const DOCUMENT: &str = "version = 3\n";

fn revision(c: char) -> SettingsRevision {
    SettingsRevision::new(c.to_string().repeat(64)).unwrap()
}

fn provider(name: &str) -> ProviderView {
    ProviderView {
        name: name.into(),
        kind: ProviderKind::OpenaiCompatible,
        base_url: "https://llm.example.invalid/v1".into(),
        credential: CredentialRef::Env { var: "KEY".into() },
        models: vec![ModelView {
            id: "m".into(),
            enabled: true,
        }],
        excluded_models: vec![],
        default_model: "m".into(),
    }
}

fn snapshot(saved: char, applied: char) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Settings(SettingsReply::Settings(Box::new(
        SettingsSnapshot {
            revision: revision(saved),
            applied_revision: revision(applied),
            document: DOCUMENT.into(),
            providers: vec![provider("local")],
            problem: None,
        },
    )))))
}

fn view(
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

fn settings_command(action: &crate::core_client::features::FeatureAction) -> &SettingsCommand {
    let Command::Settings(command) = command_of(action) else {
        panic!("a settings command was expected");
    };
    command
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
    screen.get_by_label("保存済みの設定は、まだ実行中のコアに適用されていません。");
    screen.get_by_label("local");
    drop(screen);

    let screen = loaded('a');
    screen.get_by_label("保存済みの設定は適用済みです。");
}

#[test]
fn a_save_quotes_the_revision_of_its_draft_and_is_not_sent_twice() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# edited\n");
    screen.get_by_label("保存").click();
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
    screen.get_by_label("実行中…");
    assert!(asked(&mut screen).is_empty());
}

#[test]
fn a_refused_save_keeps_the_draft_and_says_why() {
    let cases = [
        (
            ErrorCode::StaleRevision,
            "他の場所で設定が変更されました。読み込み直してから保存してください。",
        ),
        (ErrorCode::PermissionDenied, "core.restart.targets"),
    ];
    for (code, expected) in cases {
        let mut screen = loaded('a');
        type_into_document(&mut screen, "# edited\n");
        screen.get_by_label("保存").click();
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
        assert!(!screen.get_by_label("保存").accesskit_node().is_disabled());
    }
}

#[test]
fn a_finished_save_asks_for_the_reload_and_the_reload_reports_its_end() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# edited\n");
    screen.get_by_label("保存").click();
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
    screen.get_by_label("保存しました。反映するにはモデルを再読込してください。");

    screen.get_by_label("モデルを再読込").click();
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
    screen.get_by_label("モデルを再読込しました。以降のリクエストから反映されます（送信済みの内容は変わりません）。");
}

#[test]
fn a_lost_reply_or_a_reconnect_asks_for_the_operation_and_does_not_resend() {
    let mut screen = loaded('a');
    screen.get_by_label("モデルを再読込").click();
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
    screen.get_by_label("モデルを再読込しました。以降のリクエストから反映されます（送信済みの内容は変わりません）。");

    // A running operation survives a new snapshot as a question, not a command.
    let mut screen = loaded('a');
    screen.get_by_label("モデルを再読込").click();
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
    screen.get_by_label("保存").click();
    let sent = asked(&mut screen);
    let SettingsCommand::SaveDocument(save) = settings_command(&sent[0]).clone() else {
        panic!("a document save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    screen.get_by_label("実行中…");
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
    assert!(screen.query_by_label("実行中…").is_none());
    assert!(
        screen
            .query_all_by_label_contains("操作の結果を確認できませんでした")
            .next()
            .is_some()
    );
    screen.get_by_role(Role::MultilineTextInput);
    assert!(
        !screen
            .get_by_label("モデルを再読込")
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
    assert!(!screen.get_by_label("保存").accesskit_node().is_disabled());
}

#[test]
fn text_typed_while_a_save_runs_is_not_discarded_when_the_save_ends() {
    let mut screen = loaded('a');
    type_into_document(&mut screen, "# first\n");
    screen.get_by_label("保存").click();
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
    assert!(!screen.get_by_label("保存").accesskit_node().is_disabled());
    let _ = asked(&mut screen);
    screen.get_by_label("保存").click();
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
    screen.get_by_label("モデルを再読込").click();
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
    assert!(screen.query_by_label("実行中…").is_none());
    screen.get_by_label("モデルを再読込しました。以降のリクエストから反映されます（送信済みの内容は変わりません）。");
}
