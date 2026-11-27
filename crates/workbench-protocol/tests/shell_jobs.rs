mod common;
use common::*;
use workbench_protocol::{
    events::SnapshotItem,
    namespaces::execution::{ExecutionEvent, ExecutionQuery, shell::*},
};
#[test]
fn shell_observation_bounds_identity_command_and_output_ranges() {
    let mut job = common::shell_job();
    job.command = Some("c".repeat(MAX_SHELL_COMMAND_BYTES));
    SnapshotItem::ShellJob(job.clone()).validate().unwrap();
    job.command.as_mut().unwrap().push('c');
    assert!(job.validate().is_err());
    job = common::shell_job();
    job.job_uid.clear();
    assert!(job.validate().is_err());
    let mut query = ReadShellOutput {
        thread: common::thread(),
        run: common::run(),
        job_uid: "uid".into(),
        offset: 0,
        max_bytes: MAX_SHELL_TAIL_BYTES as u32,
    };
    ExecutionQuery::ReadShellOutput(query.clone())
        .validate(&Default::default())
        .unwrap();
    query.max_bytes += 1;
    assert!(query.validate().is_err());
    query.max_bytes = 0;
    assert!(query.validate().is_err());
    let event = |offset, size| ExecutionEvent::ShellJobOutput {
        thread: common::thread(),
        run: common::run(),
        job_uid: "uid".into(),
        offset,
        chunk: "o".repeat(size),
    };
    event(0, MAX_SHELL_OUTPUT_EVENT_BYTES).validate().unwrap();
    assert!(
        event(0, MAX_SHELL_OUTPUT_EVENT_BYTES + 1)
            .validate()
            .is_err()
    );
    assert!(event(u64::MAX, 1).validate().is_err());
    let mut output = ShellOutput {
        thread: common::thread(),
        run: common::run(),
        job_uid: "uid".into(),
        available_from: 4,
        offset: 4,
        chunk: "tail".into(),
        output_end: 8,
    };
    output.validate().unwrap();
    output.offset = 3;
    assert!(output.validate().is_err());
    output.offset = u64::MAX;
    assert!(output.validate().is_err());
}
