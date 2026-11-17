//! The switch from the retired todo tool to the plan tools: a retired call is
//! refused without effect, and a conversation saved before the switch is told the
//! current plan state once, never a conversation started after it.
use super::*;

const NOTICE: &str = "[runtime conversation plan state]";

fn request_json(f: &Fixture, index: usize) -> String {
    serde_json::to_string(&f.model.requests.lock().unwrap()[index]).unwrap()
}

/// One completed tool round: the model asks for `plan_read`, then waits again.
async fn tool_round(f: &Fixture) {
    f.respond(Action::Tool("plan_read", json!({"view":"tree"})));
    f.model.entered.notified().await;
}

/// Saves the run, then rewrites its record as one saved before the switch.
fn age_record(f: &Fixture, run: RunId) {
    let conn = rusqlite::Connection::open(&f.config.db_path).unwrap();
    let changed = conn
        .execute(
            "UPDATE run_contexts SET config_json = json_remove(config_json, '$.plan_notice'), config_len = length(CAST(json_remove(config_json, '$.plan_notice') AS BLOB)) WHERE run_id = ?1",
            [run.to_string()],
        )
        .unwrap();
    assert_eq!(changed, 1);
}

async fn resume(f: &Fixture, run: RunId) {
    f.runtime
        .resume_chat(
            run,
            RunConfig {
                conversation: true,
                ..Default::default()
            },
        )
        .unwrap();
    f.model.entered.notified().await;
}

#[tokio::test]
async fn a_retired_todo_write_call_is_refused_without_changing_the_plan() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.respond(Action::Tool(
        "todo_write",
        json!({"items":[{"content":"x","status":"pending"}]}),
    ));
    f.model.entered.notified().await;
    let next = request_json(&f, 1);
    assert!(
        next.contains("todo_write") && next.contains("廃止されました"),
        "{next}"
    );
    assert_eq!(read(&f, run)["revision"], 0);
    assert_eq!(read(&f, run)["nodes"], json!([]));
    f.stop(run).await;
}

#[tokio::test]
async fn a_conversation_started_after_the_switch_never_gets_the_notice() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    tool_round(&f).await;
    tool_round(&f).await;
    for index in 0..3 {
        assert!(!request_json(&f, index).contains(NOTICE), "request {index}");
    }
    f.stop(run).await;
}

#[tokio::test]
async fn a_conversation_saved_before_the_switch_is_told_the_unplanned_state_once() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.stop(run).await;
    age_record(&f, run);
    resume(&f, run).await;
    let resumed = f.model.requests.lock().unwrap().len() - 1;
    assert!(
        !request_json(&f, resumed).contains(NOTICE),
        "the resumed request itself is not changed"
    );
    tool_round(&f).await;
    let told = request_json(&f, resumed + 1);
    assert!(
        told.contains(NOTICE) && told.contains("No plan exists yet"),
        "{told}"
    );
    tool_round(&f).await;
    let again = request_json(&f, resumed + 2);
    assert_eq!(again.matches(NOTICE).count(), 1, "told exactly once");
    f.stop(run).await;
}

#[tokio::test]
async fn a_saved_conversation_with_a_plan_is_told_its_revision_and_counts() {
    let f = Fixture::new();
    let run = f.start("thread").await;
    f.add(run);
    f.stop(run).await;
    age_record(&f, run);
    resume(&f, run).await;
    let resumed = f.model.requests.lock().unwrap().len() - 1;
    tool_round(&f).await;
    let told = request_json(&f, resumed + 1);
    assert!(
        told.contains("Plan revision 2: 1 tasks") && told.contains("interrupted 1"),
        "{told}"
    );
    f.stop(run).await;
}
