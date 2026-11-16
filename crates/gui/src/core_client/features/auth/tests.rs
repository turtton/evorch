//! The auth feature drawn headless: a login is finished by pasting what the
//! browser ended on, a secret is sent once and is nowhere in what the screen
//! keeps or shows, and an operation is asked for, never sent again.
use super::*;
use crate::core_client::features::execution::tests::{
    Screen, answer, asked, command_of, query_of, screen,
};
use crate::core_client::features::{FeatureAction, FeatureError, FeatureReply, FeatureRequest};
use egui::accesskit::Role;
use egui_kittest::{
    Harness,
    kittest::{NodeT, Queryable},
};
use workbench_protocol::{commands::CommandReply, namespaces::settings::ProviderKind};

const SECRET: &str = "SENTINEL-d41d8cd98f00b204";

fn slot(
    profile: &str,
    storage: CredentialRef,
    state: CredentialState,
    login: Option<LoginMethod>,
) -> CredentialSlot {
    CredentialSlot {
        profile: profile.into(),
        kind: ProviderKind::OpenaiCompatible,
        storage,
        state,
        login,
    }
}

fn keyring(account: &str) -> CredentialRef {
    CredentialRef::Keyring {
        service: "evorch".into(),
        account: account.into(),
    }
}

fn status_reply() -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Auth(AuthReply::Status(AuthStatus {
        revision: Revision(1),
        slots: vec![
            slot(
                "claude",
                keyring("claude-acct"),
                CredentialState::Missing,
                Some(LoginMethod::Claude),
            ),
            slot(
                "key",
                keyring("key-acct"),
                CredentialState::Present {
                    expires_at_unix: None,
                },
                None,
            ),
            slot(
                "envkey",
                CredentialRef::Env {
                    var: "ENV_KEY".into(),
                },
                CredentialState::Missing,
                None,
            ),
        ],
    }))))
}

fn op_view(id: &OperationId, kind: AuthOperationKind, state: OperationState) -> AuthOperationView {
    AuthOperationView {
        operation: OperationView {
            operation_id: id.clone(),
            state,
            revision: Revision(1),
            result: None,
        },
        kind,
        profile: None,
        message: None,
    }
}

fn operation_reply(view: AuthOperationView) -> FeatureReply {
    FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Auth(
        AuthResult::Operation(view),
    ))))
}

fn input_reply(outcome: LoginInputOutcome) -> FeatureReply {
    FeatureReply::Command(Ok(CommandReply::with_result(CommandResult::Auth(
        AuthResult::LoginInput(outcome),
    ))))
}

fn login_view(id: &OperationId, phase: LoginPhase, input: bool) -> LoginView {
    LoginView {
        login_id: id.clone(),
        profile: "claude".into(),
        method: LoginMethod::Claude,
        phase,
        authorize_url: (phase == LoginPhase::Waiting)
            .then(|| "https://login.example.invalid/authorize?state=s".into()),
        accepts_input: input,
    }
}

fn login_reply(view: LoginView) -> FeatureReply {
    FeatureReply::Query(Ok(QueryReply::Auth(AuthReply::Login(view))))
}

fn auth_command(action: &FeatureAction) -> &AuthCommand {
    let Command::Auth(command) = command_of(action) else {
        panic!("an auth command was expected");
    };
    command
}

fn commands(actions: &[FeatureAction]) -> Vec<&AuthCommand> {
    actions
        .iter()
        .filter(|a| matches!(a.request, FeatureRequest::Command(_)))
        .map(auth_command)
        .collect()
}

fn loaded() -> Harness<'static, Screen> {
    let mut screen = screen(Box::<AuthFeature>::default(), None);
    let first = asked(&mut screen);
    assert_eq!(first.len(), 1);
    assert!(matches!(
        query_of(&first[0]),
        Query::Auth(AuthQuery::GetCredentialStatus {})
    ));
    answer(&mut screen, first[0].tag, status_reply());
    screen
}

/// A screen whose Claude login has been started and is waiting for a person.
fn waiting_login(input: bool) -> (Harness<'static, Screen>, OperationId) {
    let mut screen = loaded();
    screen.get_by_label("Claude でログイン").click();
    let sent = asked(&mut screen);
    let AuthCommand::StartLogin(start) = auth_command(&sent[0]).clone() else {
        panic!("a login start was expected");
    };
    assert_eq!(start.profile, "claude");
    answer(
        &mut screen,
        sent[0].tag,
        operation_reply(op_view(
            &start.operation_id,
            AuthOperationKind::Login,
            OperationState::Pending,
        )),
    );
    let queries = asked(&mut screen);
    assert!(matches!(
        query_of(&queries[0]),
        Query::Auth(AuthQuery::GetLogin { login_id }) if *login_id == start.operation_id
    ));
    answer(
        &mut screen,
        queries[0].tag,
        login_reply(login_view(&start.operation_id, LoginPhase::Waiting, input)),
    );
    (screen, start.operation_id)
}

fn type_secret(screen: &mut Harness<'static, Screen>, text: &str) {
    // The login's box, when there is one, is the last field on the screen.
    screen
        .get_all_by_role(Role::PasswordInput)
        .last()
        .unwrap()
        .focus();
    screen.run_steps(4);
    screen
        .get_all_by_role(Role::PasswordInput)
        .last()
        .unwrap()
        .type_text(text);
    screen.run_steps(4);
}

#[test]
fn every_slot_shows_its_state_and_only_what_can_be_done_with_it() {
    let screen = loaded();
    screen.get_by_label("Claude でログイン");
    screen.get_by_label("設定済み");
    screen.get_by_label("環境変数 ENV_KEY から読み込みます。");
    // A login profile takes no typed key.
    assert_eq!(screen.get_all_by_role(Role::PasswordInput).count(), 1);
}

#[test]
fn a_login_shows_its_link_and_a_box_for_what_the_browser_ended_on() {
    let (screen, _) = waiting_login(true);
    screen.get_by_label("ログインページを開く");
    screen.get_by_label("リンクをコピー");
    screen.get_by_label("送信");
    screen.get_by_label("ログインを取り消す");
}

#[test]
fn a_pasted_input_is_sent_once_and_leaves_nothing_behind() {
    let (mut screen, id) = waiting_login(true);
    type_secret(&mut screen, &format!("{SECRET}#state"));
    screen.get_by_label("送信").click();
    let sent = asked(&mut screen);
    assert_eq!(sent.len(), 1, "one click, one command");
    let AuthCommand::SubmitLoginInput(submit) = auth_command(&sent[0]) else {
        panic!("a login input was expected");
    };
    assert_eq!(submit.login_id, id);
    assert_eq!(submit.input.expose(), format!("{SECRET}#state"));
    assert!(
        !format!("{:?}", sent[0]).contains(SECRET),
        "debug output is redacted"
    );

    // The box is empty at once, before any answer: there is nothing to send again.
    assert!(screen.get_by_label("送信").accesskit_node().is_disabled());

    answer(
        &mut screen,
        sent[0].tag,
        input_reply(LoginInputOutcome::Accepted),
    );
    screen.get_by_label("入力を受け付けました。ログインを完了しています…");
    assert!(
        commands(&asked(&mut screen)).is_empty(),
        "nothing is sent again"
    );

    // The core ends the login: the screen drops it, and shows only a fixed sentence.
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Auth(AuthEvent::AuthOperationCompleted {
            operation: op_view(&id, AuthOperationKind::Login, OperationState::Ready),
        }));
    screen.run_steps(4);
    screen.get_by_label("ログインしました。");
    assert!(screen.query_by_label("送信").is_none());
}

#[test]
fn a_refused_input_says_why_and_keeps_the_login_open() {
    let cases = [
        (
            LoginInputRejection::StateMismatch,
            "このログインのものではない入力です。表示中のリンクからやり直してください。",
        ),
        (
            LoginInputRejection::AlreadyUsed,
            "このログインでは既に入力が使われています。",
        ),
        (
            LoginInputRejection::Expired,
            "ログインの有効期限が切れました。もう一度始めてください。",
        ),
        (
            LoginInputRejection::Malformed,
            "リダイレクト先のアドレス、または表示されたコードを貼り付けてください。",
        ),
    ];
    for (reason, expected) in cases {
        let (mut screen, _) = waiting_login(true);
        type_secret(&mut screen, "value");
        screen.get_by_label("送信").click();
        let sent = asked(&mut screen);
        answer(
            &mut screen,
            sent[0].tag,
            input_reply(LoginInputOutcome::Rejected { reason }),
        );
        screen.get_by_label(expected);
        screen.get_by_label("ログインを取り消す");
    }
}

#[test]
fn a_login_that_a_restart_cancelled_is_dropped_with_its_reason() {
    let (mut screen, id) = waiting_login(true);
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Auth(AuthEvent::AuthOperationCompleted {
            operation: op_view(
                &id,
                AuthOperationKind::Login,
                OperationState::Cancelled {
                    reason: CancelReason::Restart,
                },
            ),
        }));
    screen.run_steps(4);
    screen.get_by_label("再起動または終了のため中断しました。ログインはやり直してください。");
    assert!(screen.query_by_label("ログインを取り消す").is_none());
}

#[test]
fn cancelling_a_login_names_that_login_only() {
    let (mut screen, id) = waiting_login(true);
    screen.get_by_label("ログインを取り消す").click();
    let sent = asked(&mut screen);
    let [AuthCommand::CancelLogin(cancel)] = commands(&sent)[..] else {
        panic!("one cancel was expected");
    };
    assert_eq!(cancel.login_id, id);
}

#[test]
fn a_typed_key_is_sent_once_and_the_box_is_cleared() {
    let mut screen = loaded();
    type_secret(&mut screen, SECRET);
    screen.get_by_label("キーを保存").click();
    let sent = asked(&mut screen);
    let [AuthCommand::SaveApiKey(save)] = commands(&sent)[..] else {
        panic!("one key save was expected");
    };
    assert_eq!(save.profile, "key");
    assert_eq!(save.value.expose(), SECRET);
    assert!(!format!("{:?}", sent[0]).contains(SECRET));
    let id = save.operation_id.clone();
    answer(
        &mut screen,
        sent[0].tag,
        operation_reply(op_view(
            &id,
            AuthOperationKind::SaveApiKey,
            OperationState::Pending,
        )),
    );
    // Nothing is sent while it runs, and the box no longer holds the key.
    assert!(commands(&asked(&mut screen)).is_empty());
    assert!(
        screen
            .get_by_label("キーを保存")
            .accesskit_node()
            .is_disabled()
    );
}

#[test]
fn a_lost_reply_asks_for_the_operation_and_never_sends_it_again() {
    let mut screen = loaded();
    type_secret(&mut screen, "value");
    screen.get_by_label("キーを保存").click();
    let sent = asked(&mut screen);
    let AuthCommand::SaveApiKey(save) = auth_command(&sent[0]).clone() else {
        panic!("a key save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let after = asked(&mut screen);
    assert!(commands(&after).is_empty(), "no resend");
    assert!(matches!(
        query_of(after.last().unwrap()),
        Query::Auth(AuthQuery::GetAuthOperation { operation_id }) if *operation_id == save.operation_id
    ));
    answer(
        &mut screen,
        after.last().unwrap().tag,
        FeatureReply::Query(Ok(QueryReply::Auth(AuthReply::Operation(op_view(
            &save.operation_id,
            AuthOperationKind::SaveApiKey,
            OperationState::Ready,
        ))))),
    );
    screen.get_by_label("API キーを保存しました。");
}

fn not_found() -> FeatureReply {
    FeatureReply::Query(Err(
        FeatureError::new("the operation is unknown").with_code(ErrorCode::NotFound)
    ))
}

#[test]
fn a_key_save_whose_end_was_lost_with_a_core_restart_is_unknown_not_busy() {
    let mut screen = loaded();
    type_secret(&mut screen, "value");
    screen.get_by_label("キーを保存").click();
    let sent = asked(&mut screen);
    let AuthCommand::SaveApiKey(save) = auth_command(&sent[0]).clone() else {
        panic!("a key save was expected");
    };
    answer(
        &mut screen,
        sent[0].tag,
        FeatureReply::Command(Err(FeatureError::outcome_unknown("connection lost"))),
    );
    let after = asked(&mut screen);
    let query = after
        .iter()
        .find(|a| {
            matches!(
                query_of(a),
                Query::Auth(AuthQuery::GetAuthOperation { operation_id })
                    if *operation_id == save.operation_id
            )
        })
        .expect("the operation is asked for");
    assert!(
        screen
            .get_by_label("資格情報を読み直す")
            .accesskit_node()
            .is_disabled(),
        "busy while the end is unknown to the screen"
    );
    answer(&mut screen, query.tag, not_found());
    screen.run_steps(4);
    assert!(
        screen
            .query_all_by_label_contains("操作の結果を確認できませんでした")
            .next()
            .is_some()
    );
    assert!(
        !screen
            .get_by_label("資格情報を読み直す")
            .accesskit_node()
            .is_disabled()
    );
    // The credential status is read again: it shows what the save left.
    let again = asked(&mut screen);
    assert!(
        again
            .iter()
            .any(|a| matches!(query_of(a), Query::Auth(AuthQuery::GetCredentialStatus {})))
    );
}

#[test]
fn a_login_the_restarted_core_does_not_know_is_dropped_as_unknown() {
    let (mut screen, id) = waiting_login(true);
    // The connection came back to a core that restarted meanwhile.
    screen.state_mut().feature.reset();
    let questions = asked(&mut screen);
    let login = questions
        .iter()
        .find(|a| matches!(query_of(a), Query::Auth(AuthQuery::GetLogin { login_id }) if *login_id == id))
        .expect("the login is asked for");
    answer(&mut screen, login.tag, not_found());
    let operation = questions
        .iter()
        .find(|a| {
            matches!(query_of(a), Query::Auth(AuthQuery::GetAuthOperation { operation_id }) if *operation_id == id)
        })
        .expect("the operation is asked for");
    answer(&mut screen, operation.tag, not_found());
    screen.run_steps(4);
    assert!(screen.query_by_label("ログインを取り消す").is_none());
    assert!(
        screen
            .query_all_by_label_contains("操作の結果を確認できませんでした")
            .next()
            .is_some()
    );
    // The status is read again, and a new login can be started.
    let status = questions
        .iter()
        .chain(asked(&mut screen).iter())
        .find(|a| matches!(query_of(a), Query::Auth(AuthQuery::GetCredentialStatus {})))
        .map(|a| a.tag)
        .expect("the status is asked for");
    answer(&mut screen, status, status_reply());
    screen.get_by_label("Claude でログイン");
}

#[test]
fn a_pending_reply_arriving_after_the_end_does_not_make_the_screen_busy_again() {
    let mut screen = loaded();
    type_secret(&mut screen, "value");
    screen.get_by_label("キーを保存").click();
    let sent = asked(&mut screen);
    let AuthCommand::SaveApiKey(save) = auth_command(&sent[0]).clone() else {
        panic!("a key save was expected");
    };
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Auth(AuthEvent::AuthOperationCompleted {
            operation: op_view(
                &save.operation_id,
                AuthOperationKind::SaveApiKey,
                OperationState::Ready,
            ),
        }));
    answer(
        &mut screen,
        sent[0].tag,
        operation_reply(op_view(
            &save.operation_id,
            AuthOperationKind::SaveApiKey,
            OperationState::Pending,
        )),
    );
    screen.run_steps(4);
    assert!(
        !screen
            .get_by_label("資格情報を読み直す")
            .accesskit_node()
            .is_disabled()
    );
    screen.get_by_label("API キーを保存しました。");
}

#[test]
fn a_cursor_profile_takes_a_typed_api_key_next_to_its_login() {
    let mut screen = screen(Box::<AuthFeature>::default(), None);
    let first = asked(&mut screen);
    answer(
        &mut screen,
        first[0].tag,
        FeatureReply::Query(Ok(QueryReply::Auth(AuthReply::Status(AuthStatus {
            revision: Revision(1),
            slots: vec![slot(
                "cursor",
                keyring("cursor-acct"),
                CredentialState::Missing,
                Some(LoginMethod::Cursor),
            )],
        })))),
    );
    screen.get_by_label("Cursor でログイン");
    assert_eq!(screen.get_all_by_role(Role::PasswordInput).count(), 1);
    type_secret(&mut screen, SECRET);
    screen.get_by_label("キーを保存").click();
    let sent = asked(&mut screen);
    let [AuthCommand::SaveApiKey(save)] = commands(&sent)[..] else {
        panic!("one key save was expected");
    };
    assert_eq!(save.profile, "cursor");
    assert_eq!(save.value.expose(), SECRET);
}

/// A login that is still being prepared has an operation but no session yet:
/// the question for the session is answered `not_found`, which must not end
/// the screen's tracking. The core announces the session with `login_changed`
/// and the link is then shown.
#[test]
fn a_login_still_being_prepared_is_not_taken_for_lost() {
    let mut screen = loaded();
    screen.get_by_label("Claude でログイン").click();
    let sent = asked(&mut screen);
    let AuthCommand::StartLogin(start) = auth_command(&sent[0]).clone() else {
        panic!("a login start was expected");
    };
    let id = start.operation_id;
    answer(
        &mut screen,
        sent[0].tag,
        operation_reply(op_view(
            &id,
            AuthOperationKind::Login,
            OperationState::Pending,
        )),
    );
    let questions = asked(&mut screen);
    let login = questions
        .iter()
        .find(|a| matches!(query_of(a), Query::Auth(AuthQuery::GetLogin { login_id }) if *login_id == id))
        .expect("the login is asked for");
    // The core has not registered the session yet.
    answer(&mut screen, login.tag, not_found());
    let after = asked(&mut screen);
    let operation = after
        .iter()
        .find(|a| {
            matches!(query_of(a), Query::Auth(AuthQuery::GetAuthOperation { operation_id }) if *operation_id == id)
        })
        .expect("the operation is asked for");
    answer(
        &mut screen,
        operation.tag,
        operation_reply(op_view(
            &id,
            AuthOperationKind::Login,
            OperationState::Pending,
        )),
    );
    screen.run_steps(4);
    // Still tracked as a login in preparation: nothing says it is lost.
    assert!(
        screen
            .query_all_by_label_contains("操作の結果を確認できませんでした")
            .next()
            .is_none()
    );
    screen.get_by_label("ログインを準備しています。");

    // The core announces the session, and the link is shown.
    screen
        .state_mut()
        .feature
        .event(&UiEvent::Auth(AuthEvent::LoginChanged {
            login_id: id.clone(),
        }));
    screen.run_steps(4);
    let again = asked(&mut screen);
    let login = again
        .iter()
        .find(|a| matches!(query_of(a), Query::Auth(AuthQuery::GetLogin { login_id }) if *login_id == id))
        .expect("the login is asked for again");
    answer(
        &mut screen,
        login.tag,
        login_reply(login_view(&id, LoginPhase::Waiting, true)),
    );
    screen.run_steps(4);
    screen.get_by_label("ログインページを開く");
}
