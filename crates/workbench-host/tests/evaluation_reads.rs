//! The `evaluation` reads (PR 13) over evaluation files on disk: a file that is
//! not there is told apart from one that is unusable, no host path reaches a
//! reply, and reading never creates or changes a file.
#[path = "memory_reads/support.rs"]
mod support;

use std::path::Path;

use serde_json::json;
use support::{World, operation, ready, world};
use workbench_host::areas::{Areas, evaluation::EvaluationArea};
use workbench_protocol::WorkbenchError;
use workbench_protocol::errors::ErrorCode;
use workbench_protocol::ids::{EvaluationGroupId, EvaluationMemberId};
use workbench_protocol::management::ArtifactPresence;
use workbench_protocol::namespaces::diagnostics::ReadOutcome;
use workbench_protocol::namespaces::evaluation::{
    EvaluationGroupDetail, EvaluationGroupPhase, EvaluationList, EvaluationProblemKind,
    EvaluationQuery, EvaluationReply, EvaluationReportText, GetEvaluation, GetEvaluationReport,
    ListEvaluations,
};
use workbench_protocol::operation::OperationState;
use workbench_protocol::queries::{PageRequest, Query, QueryReply};

const GOOD: &str = "20261020-000000-abc123";
const BROKEN: &str = "20261019-000000-def456";

fn evaluation_world() -> World {
    world(|parts| Areas {
        evaluation: EvaluationArea::from_parts(
            parts.tasks,
            parts.projection.source("evaluation").unwrap(),
            parts.dir.join("eval"),
        ),
        ..Default::default()
    })
}

fn member(root: &Path, group: &str, name: &str, finished: bool) -> serde_json::Value {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    let group_dir = root.join(".groups").join(group);
    json!({
        "thread_id": format!("thread-{name}"),
        "purpose": "target",
        "repeat": 1,
        "dir": dir,
        "pid": null,
        "stdout": group_dir.join(format!("{name}.stdout.log")),
        "stderr": group_dir.join(format!("{name}.stderr.log")),
        "exit": finished.then(|| json!({
            "code": 0, "signal": null, "error": null, "finished_at": "2026-10-20T00:01:00.000Z"
        })),
    })
}

/// A group of three members: one with a report, one with none, one whose
/// `report.md` is a link out of the evaluation root.
fn seed(w: &World) {
    let root = w.dir.path().join("eval");
    let group_dir = root.join(".groups").join(GOOD);
    std::fs::create_dir_all(&group_dir).unwrap();
    std::fs::write(group_dir.join("candidate.toml"), "[agents.worker]\n").unwrap();
    let members = [
        member(&root, GOOD, "member-a", true),
        member(&root, GOOD, "member-b", true),
        member(&root, GOOD, "member-c", true),
    ];
    std::fs::write(
        root.join("member-a/report.md"),
        format!("# Report\n\ntoken sk-{}\n", "a1B2c3D4e5F6g7H8i9J0k1L2"),
    )
    .unwrap();
    let outside = w.dir.path().join("outside.md");
    std::fs::write(&outside, "# not part of the evaluation").unwrap();
    std::os::unix::fs::symlink(&outside, root.join("member-c/report.md")).unwrap();
    std::fs::write(
        group_dir.join("group.json"),
        serde_json::to_vec(&json!({
            "schema": 1,
            "id": GOOD,
            "run_id": "run-7",
            "started_at": "2026-10-20T00:00:00.000Z",
            "candidate": group_dir.join("candidate.toml"),
            "members": members,
            "finished_at": "2026-10-20T00:02:00.000Z",
            "notice": null,
        }))
        .unwrap(),
    )
    .unwrap();

    let broken = root.join(".groups").join(BROKEN);
    std::fs::create_dir_all(&broken).unwrap();
    std::fs::write(broken.join("group.json"), "{ not json").unwrap();
}

fn list(w: &World, id: &str) -> Result<ReadOutcome<EvaluationList>, WorkbenchError> {
    match w.query(Query::Evaluation(EvaluationQuery::ListEvaluations(
        ListEvaluations {
            operation_id: operation(id),
            page: PageRequest::default(),
        },
    )))? {
        QueryReply::Evaluation(EvaluationReply::List(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn group(
    w: &World,
    id: &str,
    group: &str,
) -> Result<ReadOutcome<EvaluationGroupDetail>, WorkbenchError> {
    match w.query(Query::Evaluation(EvaluationQuery::GetEvaluation(
        GetEvaluation {
            operation_id: operation(id),
            group: EvaluationGroupId::new(group).unwrap(),
        },
    )))? {
        QueryReply::Evaluation(EvaluationReply::Group(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

fn report(
    w: &World,
    id: &str,
    member: &str,
) -> Result<ReadOutcome<EvaluationReportText>, WorkbenchError> {
    match w.query(Query::Evaluation(EvaluationQuery::GetEvaluationReport(
        GetEvaluationReport {
            operation_id: operation(id),
            group: EvaluationGroupId::new(GOOD).unwrap(),
            member: EvaluationMemberId::new(member).unwrap(),
        },
    )))? {
        QueryReply::Evaluation(EvaluationReply::Report(outcome)) => Ok(*outcome),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_broken_group_is_listed_as_a_problem_next_to_a_good_one() {
    let w = evaluation_world();
    seed(&w);
    let outcome = w.read(|| list(&w, "list"));
    assert!(ready(&outcome.operation.state));
    let groups = outcome.data.unwrap().groups.items;
    assert_eq!(groups.len(), 2);
    let good = groups.iter().find(|g| g.group.as_str() == GOOD).unwrap();
    assert_eq!(good.presence, ArtifactPresence::Present);
    assert_eq!(good.members_total, Some(3));
    let broken = groups.iter().find(|g| g.group.as_str() == BROKEN).unwrap();
    assert_eq!(broken.presence, ArtifactPresence::Problem);
    assert_eq!(broken.problem, Some(EvaluationProblemKind::Malformed));
    assert_eq!(broken.phase, None);
    assert_ne!(good.revision, broken.revision);
}

#[test]
fn a_member_report_is_present_missing_or_a_problem_and_never_a_guess() {
    let w = evaluation_world();
    seed(&w);
    let detail = w
        .read(|| group(&w, "group", GOOD))
        .data
        .expect("the group is read");
    assert_eq!(detail.members.len(), 3);
    let presence = |name: &str| {
        detail
            .members
            .iter()
            .find(|m| m.member.as_str() == name)
            .unwrap()
            .report
            .presence
    };
    assert_eq!(presence("member-a"), ArtifactPresence::Present);
    assert_eq!(presence("member-b"), ArtifactPresence::Missing);
    assert_eq!(presence("member-c"), ArtifactPresence::Problem);
    assert_ne!(detail.phase, EvaluationGroupPhase::Running);
    assert_eq!(detail.members_total, 3);
}

#[test]
fn a_report_body_is_masked_and_a_link_out_of_the_root_is_refused() {
    let w = evaluation_world();
    seed(&w);
    let present = w.read(|| report(&w, "report-a", "member-a")).data.unwrap();
    assert_eq!(present.artifact.presence, ArtifactPresence::Present);
    let text = present.text.unwrap();
    assert!(text.text.starts_with("# Report"));
    assert!(
        !text.text.contains("a1B2c3D4e5F6g7H8i9J0k1L2"),
        "{}",
        text.text
    );

    let missing = w.read(|| report(&w, "report-b", "member-b")).data.unwrap();
    assert_eq!(missing.artifact.presence, ArtifactPresence::Missing);
    assert!(missing.text.is_none() && missing.problem.is_none());

    let linked = w.read(|| report(&w, "report-c", "member-c")).data.unwrap();
    assert_eq!(linked.artifact.presence, ArtifactPresence::Problem);
    assert!(linked.text.is_none());
    assert_eq!(linked.problem, Some(EvaluationProblemKind::UnsafePath));
    assert!(
        !serde_json::to_string(&linked)
            .unwrap()
            .contains("not part of the evaluation")
    );

    let unknown = w.read(|| report(&w, "report-x", "member-zzz"));
    assert_eq!(
        unknown.operation.state,
        OperationState::Failed {
            code: ErrorCode::NotFound
        }
    );
}

#[test]
fn no_host_path_reaches_a_reply_and_nothing_is_written() {
    let w = evaluation_world();
    seed(&w);
    let root = w.dir.path().to_path_buf();
    let snapshot = |dir: &Path| {
        let mut entries = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            for entry in std::fs::read_dir(&path).unwrap().flatten() {
                let meta = entry.metadata().unwrap();
                entries.push((entry.path(), meta.len(), meta.modified().unwrap()));
                if meta.is_dir() {
                    stack.push(entry.path());
                }
            }
        }
        entries.sort();
        entries
    };
    let before = snapshot(&root.join("eval"));
    let listed = w.read(|| list(&w, "paths-list"));
    let detail = w.read(|| group(&w, "paths-group", GOOD));
    let wire = format!(
        "{}{}",
        serde_json::to_string(&listed).unwrap(),
        serde_json::to_string(&detail).unwrap()
    );
    assert!(!wire.contains(root.to_str().unwrap()), "{wire}");
    assert_eq!(snapshot(&root.join("eval")), before);
}

#[test]
fn an_unknown_group_fails_as_not_found_and_an_unwired_area_is_unsupported() {
    let w = evaluation_world();
    seed(&w);
    let outcome = w.read(|| group(&w, "nope", "20200101-000000-000000"));
    assert!(outcome.data.is_none());
    assert_eq!(
        outcome.operation.state,
        OperationState::Failed {
            code: ErrorCode::NotFound
        }
    );
    let empty = evaluation_world();
    let listed = empty.read(|| list(&empty, "empty")).data.unwrap();
    assert!(listed.groups.items.is_empty());

    let unwired = world(|_| Areas::default());
    assert_eq!(
        list(&unwired, "x").unwrap_err().code,
        ErrorCode::UnsupportedCapability
    );
}

/// A file `limit` bytes into which a credential runs `keep` bytes: the read stops
/// inside it. `before` stands right in front of the credential, with a space before
/// that when `spaced`.
fn straddling(limit: usize, before: &str, prefix: &str, keep: usize, spaced: bool) -> String {
    let rest = "Zq8Wm3Nv7Xc2Rb5Ty9Uh4Jk6Lp1Sd0FgAaBbCcDd";
    let credential = format!("{prefix}{rest}more text after the credential\n");
    let lead = limit - keep - usize::from(spaced);
    // Words with spaces between them, or one unbroken run when there is no space to find.
    let lead = if spaced {
        "x ".repeat(lead / 2) + &"x".repeat(lead % 2)
    } else {
        "x".repeat(lead)
    };
    format!(
        "{lead}{}{before}{credential}",
        if spaced { " " } else { "" }
    )
}

#[test]
fn a_credential_cut_by_the_size_limit_leaves_no_fragment_in_a_report_or_a_candidate() {
    let w = evaluation_world();
    seed(&w);
    let root = w.dir.path().join("eval");
    let candidate = root.join(".groups").join(GOOD).join("candidate.toml");
    let report_limit = 65_536 - 64;
    let candidate_limit = 4 * 1024;
    // The detector reads a word's opening through the character before it, so the
    // fragment must be gone whatever that character is, and with no space at all.
    for (round, (before, prefix, keep, spaced)) in [
        ("", "sk-", 15, true),
        (".", "sk-", 15, true),
        ("-", "ghp_", 22, true),
        ("_", "ghp_", 33, true),
        ("a", "sk-", 22, true),
        ("", "sk-", 15, false),
        (".", "ghp_", 33, false),
    ]
    .into_iter()
    .enumerate()
    {
        // Given: a report and an overlay whose credential the size limit cuts.
        std::fs::write(
            root.join("member-a/report.md"),
            straddling(report_limit, before, prefix, keep, spaced),
        )
        .unwrap();
        std::fs::write(
            &candidate,
            straddling(candidate_limit, before, prefix, keep, spaced),
        )
        .unwrap();

        // When: both are read.
        let report_text = w
            .read(|| report(&w, &format!("cut-report-{round}"), "member-a"))
            .data
            .unwrap()
            .text
            .unwrap();
        let candidate_text = w
            .read(|| group(&w, &format!("cut-group-{round}"), GOOD))
            .data
            .unwrap()
            .candidate
            .unwrap();

        // Then: no fragment of the credential is shown, and both say they were cut.
        for text in [&report_text, &candidate_text] {
            let label = format!("{before:?} {prefix:?} {keep} {spaced}");
            assert!(text.truncated, "{label}");
            assert!(!text.text.contains("Zq8W"), "{label}");
            assert!(!text.text.contains(prefix), "{label}");
            assert!(!text.text.contains("more text"), "{label}");
        }
    }
}
