//! `todo_write` operations: exactly one of replace, advance or update, and the
//! revision discipline around them.
use super::*;
use ThreadTodoStatus::{Completed, InProgress, Interrupted, Pending};

fn list(statuses: &[ThreadTodoStatus]) -> Vec<ThreadTodoItem> {
    statuses
        .iter()
        .enumerate()
        .map(|(index, status)| item(&format!("step {index}"), *status))
        .collect()
}

fn statuses(items: &[ThreadTodoItem]) -> Vec<ThreadTodoStatus> {
    items.iter().map(|item| item.status).collect()
}

fn advanced(before: &[ThreadTodoStatus]) -> Result<Option<Vec<ThreadTodoStatus>>, String> {
    advance(&list(before)).map(|next| next.map(|items| statuses(&items)))
}

#[test]
fn advance_completes_the_single_in_progress_item_and_starts_the_first_pending_in_list_order() {
    // The first pending item in array order starts, even before the finished one.
    assert_eq!(
        advanced(&[Completed, Pending, InProgress, Pending]),
        Ok(Some(vec![Completed, InProgress, Completed, Pending]))
    );
    assert_eq!(
        advanced(&[InProgress, Pending, Pending]),
        Ok(Some(vec![Completed, InProgress, Pending]))
    );
    // Nothing left to start: the last item just completes.
    assert_eq!(
        advanced(&[Completed, InProgress]),
        Ok(Some(vec![Completed, Completed]))
    );
    // An interrupted item elsewhere does not block an unambiguous advance.
    assert_eq!(
        advanced(&[Interrupted, InProgress, Pending]),
        Ok(Some(vec![Interrupted, Completed, InProgress]))
    );
}

#[test]
fn advance_without_work_in_progress_starts_the_first_pending_item() {
    assert_eq!(
        advanced(&[Completed, Pending, Pending]),
        Ok(Some(vec![Completed, InProgress, Pending]))
    );
}

#[test]
fn advance_refuses_ambiguity_and_reports_a_finished_list_as_a_no_op() {
    let several = advanced(&[InProgress, InProgress, Pending]).unwrap_err();
    assert!(several.contains("update"), "{several}");
    let undecided = advanced(&[Interrupted, Pending]).unwrap_err();
    assert!(undecided.contains("update"), "{undecided}");
    assert_eq!(advanced(&[Completed, Completed]), Ok(None));
    assert!(advance(&[]).is_err());
}

#[test]
fn update_sets_one_status_and_reports_the_same_status_as_no_change() {
    let items = list(&[Pending, Interrupted, Completed]);
    let next = update(&items, 1, InProgress).unwrap().unwrap();
    assert_eq!(statuses(&next), vec![Pending, InProgress, Completed]);
    assert_eq!(
        update(&items, 1, Interrupted).unwrap_err(),
        INTERRUPTED_IS_RUNTIME_ONLY
    );
    assert_eq!(update(&items, 2, Completed), Ok(None));
    let range = update(&items, 3, Completed).unwrap_err();
    assert!(range.contains("out of range"), "{range}");
    assert!(update(&[], 0, Pending).is_err());
}

#[test]
fn exactly_one_well_formed_operation_is_accepted() {
    let items = json!([{"content":"a","status":"pending"}]);
    assert!(matches!(
        parse_operation(json!({"items":items})),
        Ok(Operation::Replace(_))
    ));
    assert!(matches!(
        parse_operation(json!({"items":[]})),
        Ok(Operation::Replace(items)) if items.is_empty()
    ));
    assert!(matches!(
        parse_operation(json!({"advance":true,"expected_revision":4})),
        Ok(Operation::Advance {
            expected_revision: 4
        })
    ));
    assert!(matches!(
        parse_operation(json!({"update":{"index":2,"status":"completed"},"expected_revision":0})),
        Ok(Operation::Update {
            index: 2,
            status: Completed,
            expected_revision: 0
        })
    ));
    for invalid in [
        json!({}),
        json!([]),
        json!({"items":items,"advance":true,"expected_revision":1}),
        json!({"items":items,"update":{"index":0,"status":"pending"},"expected_revision":1}),
        json!({"advance":true,"update":{"index":0,"status":"pending"},"expected_revision":1}),
        json!({"advance":false,"expected_revision":1}),
        json!({"advance":"true","expected_revision":1}),
        json!({"advance":null,"expected_revision":1}),
        json!({"advance":true}),
        json!({"advance":true,"expected_revision":-1}),
        json!({"advance":true,"expected_revision":"1"}),
        json!({"advance":true,"expected_revision":1.5}),
        json!({"items":items,"expected_revision":1}),
        json!({"items":null}),
        json!({"update":{"index":0,"status":"pending"}}),
        json!({"update":{"index":-1,"status":"pending"},"expected_revision":1}),
        json!({"update":{"index":"0","status":"pending"},"expected_revision":1}),
        json!({"update":{"index":0,"status":"started"},"expected_revision":1}),
        json!({"update":{"index":0},"expected_revision":1}),
        json!({"update":{"index":0,"status":"pending","note":"x"},"expected_revision":1}),
        json!({"advance":true,"expected_revision":1,"extra":1}),
    ] {
        assert!(parse_operation(invalid.clone()).is_err(), "{invalid}");
    }
}

#[test]
fn the_runtime_only_interrupted_status_is_rejected_from_model_input() {
    for input in [
        json!({"items":[{"content":"a","status":"interrupted"}]}),
        json!({"update":{"index":0,"status":"interrupted"},"expected_revision":1}),
    ] {
        assert_eq!(
            parse_operation(input).unwrap_err(),
            INTERRUPTED_IS_RUNTIME_ONLY
        );
    }
    // Persisted lists may hold it: restore is the runtime's own data path.
    let (runtime, _, _) = harness(vec![]);
    assert!(
        runtime
            .restore_thread_todo(restored(vec![item("a", Interrupted)]))
            .is_ok()
    );
}

#[test]
fn the_schema_is_one_fixed_object_that_never_offers_interrupted() {
    let schema = spec("todo_write").input_schema;
    assert_eq!(schema["type"], "object");
    // Exactly-one is checked in Rust, so the schema offers no choice keyword.
    assert_eq!(schema["required"], json!([]));
    for key in ["oneOf", "anyOf", "allOf"] {
        assert!(schema.get(key).is_none(), "{key}");
    }
    let properties = schema["properties"].as_object().unwrap();
    assert_eq!(
        properties.keys().map(String::as_str).collect::<Vec<_>>(),
        ["advance", "expected_revision", "items", "update"]
    );
    assert!(!schema.to_string().contains("interrupted"));
    // Nothing in the description depends on this list's state.
    assert_eq!(
        spec("todo_write").description,
        spec("todo_write").description
    );
    assert!(!spec("todo_write").description.contains("must continue"));
}

fn call(input: Value) -> Step {
    Step::Tools(vec![("todo_write", input)])
}

fn parsed(result: &(bool, String)) -> Value {
    serde_json::from_str(&result.1).unwrap_or_else(|_| json!(result.1))
}

#[tokio::test]
async fn operations_progress_a_list_and_a_stale_resend_cannot_progress_it_twice() {
    let (runtime, model, _) = harness(vec![
        write(list(&[Pending, Pending])),
        call(json!({"advance":true,"expected_revision":1})),
        call(json!({"update":{"index":1,"status":"in_progress"},"expected_revision":2})),
        // Two items run in parallel: advance is ambiguous.
        call(json!({"advance":true,"expected_revision":3})),
        call(json!({"update":{"index":0,"status":"completed"},"expected_revision":3})),
        // A retry of an earlier advance names an old revision.
        call(json!({"advance":true,"expected_revision":3})),
        call(json!({"advance":true,"expected_revision":4})),
        // Everything is done: no-ops keep the revision.
        call(json!({"advance":true,"expected_revision":5})),
        call(json!({"update":{"index":0,"status":"completed"},"expected_revision":5})),
        call(json!({"update":{"index":2,"status":"pending"},"expected_revision":5})),
        call(json!({"update":{"index":0,"status":"interrupted"},"expected_revision":5})),
        Step::Stop,
    ]);
    let run = start(&runtime, Role::Worker, config(), true);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let requests = model.requests.lock().unwrap();
    let results = results(&requests);
    let ok = |index: usize| {
        assert!(!results[index].0, "{index}: {}", results[index].1);
        parsed(&results[index])
    };
    assert_eq!(ok(0)["revision"], 1);
    let started = ok(1);
    assert_eq!(
        (started["revision"].clone(), started["changed"].clone()),
        (json!(2), json!(true))
    );
    assert_eq!(started["current"], json!([{"index":0,"content":"step 0"}]));
    assert_eq!(
        (ok(2)["revision"].clone(), ok(2)["in_progress"].clone()),
        (json!(3), json!(2))
    );
    assert!(
        results[3].0 && results[3].1.contains("update"),
        "{}",
        results[3].1
    );
    assert_eq!(ok(4)["revision"], 4);
    // Stale: refused, with the current revision and list so the model can recover.
    assert!(results[5].0);
    assert!(
        results[5].1.contains("current revision is 4"),
        "{}",
        results[5].1
    );
    assert!(results[5].1.contains("[1] in_progress"), "{}", results[5].1);
    let finished = ok(6);
    assert_eq!(
        (finished["revision"].clone(), finished["completed"].clone()),
        (json!(5), json!(2))
    );
    assert_eq!(finished["current"], json!([]));
    for index in [7, 8] {
        let unchanged = ok(index);
        assert_eq!(
            (unchanged["revision"].clone(), unchanged["changed"].clone()),
            (json!(5), json!(false))
        );
    }
    assert!(results[9].0 && results[9].1.contains("out of range"));
    // The schema itself already refuses it before dispatch.
    assert!(
        results[10].0 && results[10].1.contains("interrupted"),
        "{:?}",
        results[10]
    );
    // A completed list has nothing to interrupt when the run ends.
    let snapshot = runtime.thread_todo("thread").unwrap();
    assert_eq!(snapshot.revision, 5);
    assert_eq!(statuses(&snapshot.items), vec![Completed, Completed]);
}

#[tokio::test]
async fn a_cleared_list_stays_cleared_and_cannot_be_advanced_or_updated() {
    let (runtime, model, _) = harness(vec![
        write(list(&[Pending])),
        write(vec![]),
        call(json!({"advance":true,"expected_revision":2})),
        call(json!({"update":{"index":0,"status":"completed"},"expected_revision":2})),
        Step::Stop,
    ]);
    let run = start(&runtime, Role::Worker, config(), true);
    assert_eq!(runtime.wait(run).await.unwrap(), AgentRunPhase::Done);
    let requests = model.requests.lock().unwrap();
    let results = results(&requests);
    assert!(!results[1].0);
    assert!(results[2].0 && results[3].0);
    let snapshot = runtime.thread_todo("thread").unwrap();
    assert_eq!((snapshot.revision, snapshot.items.len()), (2, 0));
}

#[test]
fn operations_need_a_registered_list_and_never_wrap_the_revision() {
    let (runtime, _, _) = harness(vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    let unregistered = runtime
        .write_thread_todo(
            root,
            Operation::Advance {
                expected_revision: 0,
            },
        )
        .unwrap_err();
    assert!(unregistered.contains("no procedure"), "{unregistered}");
    assert!(runtime.thread_todo("thread").is_none());

    let mut snapshot = restored(list(&[Pending, Completed]));
    snapshot.revision = u64::MAX;
    runtime.restore_thread_todo(snapshot.clone()).unwrap();
    for operation in [
        Operation::Advance {
            expected_revision: u64::MAX,
        },
        Operation::Update {
            index: 0,
            status: Completed,
            expected_revision: u64::MAX,
        },
        Operation::Replace(vec![]),
    ] {
        let error = runtime.write_thread_todo(root, operation).unwrap_err();
        assert!(error.contains("limit"), "{error}");
        assert_eq!(runtime.thread_todo("thread"), Some(snapshot.clone()));
    }
    // A request that changes nothing needs no new revision.
    let unchanged = runtime
        .write_thread_todo(
            root,
            Operation::Update {
                index: 1,
                status: Completed,
                expected_revision: u64::MAX,
            },
        )
        .unwrap();
    assert!(!unchanged.changed);
    assert_eq!(runtime.thread_todo("thread"), Some(snapshot));
}

#[test]
fn stale_revisions_change_nothing_and_publish_nothing() {
    let (runtime, _, mut events) = harness(vec![]);
    let root = runtime.reserve_run_id();
    runtime.bind_thread_root("thread", root).unwrap();
    runtime
        .restore_thread_todo(restored(list(&[Pending, InProgress])))
        .unwrap();
    let before = runtime.thread_todo("thread");
    for expected_revision in [0, 2, 4] {
        let error = runtime
            .write_thread_todo(root, Operation::Advance { expected_revision })
            .unwrap_err();
        assert!(error.contains("current revision is 3"), "{error}");
    }
    assert_eq!(runtime.thread_todo("thread"), before);
    assert!(events.drain_pending_snapshot().is_empty());
}
