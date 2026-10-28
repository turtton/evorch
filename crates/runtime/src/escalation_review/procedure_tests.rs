//! What the reviewer sees of loaded skills, and when a verified procedure step
//! lets a critical-risk shell command run.

use super::*;
use crate::rules::{ProjectTrust, RulesSettings, RulesSource};
use crate::skill::SkillScope;
use providers::FinishReason;
use support::{ScriptedModel, text_response};
use tools::tools::shell_escalation::{FileAccessRequest, ShellAccess};

const CLEANUP_PROCEDURE: &str = "After the pull request is merged, remove the task worktree and its branch, but only after checking that the branch head equals the pull request's final head.";

fn run_with_skills(skills: Vec<LoadedSkill>) -> ReviewRunContext {
    let mut run = ReviewRunContext::empty("run-1");
    run.add_user_request("run-1", "CI通ったらマージしていいよ");
    for skill in skills {
        run.record_loaded_skill(skill);
    }
    run
}

fn cleanup_skill(scope: SkillScope) -> LoadedSkill {
    LoadedSkill {
        name: "worktree-pr".into(),
        scope,
        loaded_by: LoadedBy::SkillLoad,
        text: CLEANUP_PROCEDURE.into(),
    }
}

fn rules_source(project: &std::path::Path, trust: ProjectTrust) -> RulesSource {
    RulesSource::new(
        trust,
        RulesSettings {
            context_window_tokens: 0,
            response_headroom_tokens: 0,
            max_injection_bytes: 0,
        },
        None,
        Some(project.to_path_buf()),
        None,
    )
}

fn context_with_skill() -> ReviewContext {
    review_context(
        run_with_skills(vec![cleanup_skill(SkillScope::Builtin)]),
        None,
    )
    .unwrap()
    .0
}

fn bare_context() -> ReviewContext {
    review_context(run_with_skills(vec![]), None).unwrap().0
}

fn context_with_rules_only() -> ReviewContext {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "Clean up after merging").unwrap();
    let rules = rules_source(project.path(), ProjectTrust::Approved);
    review_context(run_with_skills(vec![]), Some(&rules))
        .unwrap()
        .0
}

fn response(approve: bool, risk: &str, authorization: &str, step: Option<&str>) -> String {
    let step = step
        .map(|step| format!(r#","procedure_step":"{step}""#))
        .unwrap_or_default();
    format!(
        r#"{{"approve":{approve},"reason":"cleanup","risk_level":"{risk}","authorization_level":"{authorization}"{step}}}"#
    )
}

async fn shell(context: ReviewContext, response: &str) -> ReviewVerdict {
    QuickModelReviewer::new(Arc::new(ScriptedModel::new([Ok(text_response(
        response,
        FinishReason::Stop,
    ))])))
    .review_scoped_with_context(
        "run-1",
        "cleanup",
        "cleanup",
        Some(context),
        ShellAccess::Host,
    )
    .await
    .unwrap()
}

fn requires_user() -> ReviewVerdict {
    ReviewVerdict::RequiresUser {
        reason: "critical-risk shell action requires user approval".into(),
    }
}

#[tokio::test]
async fn a_verified_step_lets_a_critical_shell_command_run() {
    for authorization in ["medium", "high"] {
        let verified = response(true, "critical", authorization, Some("verified"));
        assert_eq!(
            shell(context_with_skill(), &verified).await,
            ReviewVerdict::Approve,
            "{authorization} with a loaded skill"
        );
        assert_eq!(
            shell(context_with_rules_only(), &verified).await,
            ReviewVerdict::Approve,
            "{authorization} with project rules"
        );
    }
}

#[tokio::test]
async fn every_other_critical_shell_case_still_goes_to_the_user() {
    let cases = [
        (
            "no procedure in context",
            bare_context(),
            "medium",
            Some("verified"),
        ),
        (
            "low authorization",
            context_with_skill(),
            "low",
            Some("verified"),
        ),
        (
            "none authorization",
            context_with_skill(),
            "none",
            Some("verified"),
        ),
        (
            "defined only",
            context_with_skill(),
            "high",
            Some("defined"),
        ),
        ("no step", context_with_skill(), "high", Some("none")),
        ("step missing", context_with_skill(), "high", None),
    ];
    for (name, context, authorization, step) in cases {
        assert_eq!(
            shell(context, &response(true, "critical", authorization, step)).await,
            requires_user(),
            "{name}"
        );
    }
}

#[tokio::test]
async fn a_denial_stays_a_denial_even_for_a_verified_step() {
    assert_eq!(
        shell(
            context_with_skill(),
            &response(false, "critical", "high", Some("verified"))
        )
        .await,
        ReviewVerdict::Deny {
            reason: "cleanup".into()
        }
    );
}

#[tokio::test]
async fn the_step_does_not_change_non_critical_thresholds() {
    // A verified step does not lift the authorization needed for high risk.
    assert_eq!(
        shell(
            context_with_skill(),
            &response(true, "high", "low", Some("verified"))
        )
        .await,
        ReviewVerdict::Deny {
            reason: "high-risk shell action lacks sufficient real-user authorization".into()
        }
    );
}

#[tokio::test]
async fn verified_means_nothing_for_file_web_and_upload_reviews() {
    let verified = response(true, "critical", "high", Some("verified"));
    let reviewer = || {
        QuickModelReviewer::new(Arc::new(ScriptedModel::new([Ok(text_response(
            &verified,
            FinishReason::Stop,
        ))])))
    };
    let file = reviewer()
        .review_file_access(
            "run-1",
            &FileAccessRequest {
                tool: "write".into(),
                paths: vec!["/home/user/.ssh/config".into()],
                write: true,
                justification: "configure".into(),
                change: Some("Host *".into()),
            },
            Some(context_with_skill()),
        )
        .await;
    let web = reviewer()
        .review_web_fetch(
            "run-1",
            &serde_json::json!({"url":"https://example.com/?token=secret"}),
            Some(context_with_skill()),
        )
        .await;
    let upload = reviewer()
        .review_upload(
            "run-1",
            serde_json::json!({"form_origin":"https://example.com","files":[]}),
            Some(context_with_skill()),
        )
        .await;
    for (action, result) in [
        ("file access", file),
        ("web fetch", web),
        ("upload", upload),
    ] {
        assert_eq!(
            result,
            Ok(ReviewVerdict::RequiresUser {
                reason: format!("critical-risk {action} requires user approval")
            }),
            "{action}"
        );
    }
}

#[tokio::test]
async fn the_cleanup_procedure_reaches_the_reviewer_next_to_the_user_request() {
    // Given: a user who allowed merging, an approved AGENTS.md, and a skill the
    // agent read that defines the cleanup after the merge.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("AGENTS.md"),
        "Open a PR for code changes",
    )
    .unwrap();
    let rules = rules_source(project.path(), ProjectTrust::Approved);
    let context = review_context(
        run_with_skills(vec![cleanup_skill(SkillScope::Builtin)]),
        Some(&rules),
    )
    .unwrap()
    .0;
    let model = Arc::new(ScriptedModel::new([Ok(text_response(
        &response(true, "critical", "high", Some("verified")),
        FinishReason::Stop,
    ))]));
    let verdict = QuickModelReviewer::new(model.clone())
        .review_scoped_with_context(
            "run-1",
            "git worktree remove .worktrees/task",
            "remove the merged task worktree",
            Some(context),
            ShellAccess::Host,
        )
        .await;
    assert_eq!(verdict, Ok(ReviewVerdict::Approve));

    // Then: the payload carries the user's words, the rules, and the skill body.
    let observed = model.observed().await;
    let [ContentBlock::Text { text }] = observed[0][1].content.as_slice() else {
        panic!("expected one text block");
    };
    let payload: serde_json::Value = serde_json::from_str(text).unwrap();
    let context = &payload["context"];
    assert_eq!(
        context["real_user_requests"][0]["text"],
        "CI通ったらマージしていいよ"
    );
    assert_eq!(
        context["project_rules"]["text"],
        "Open a PR for code changes"
    );
    assert_eq!(context["loaded_skills"][0]["name"], "worktree-pr");
    assert_eq!(context["loaded_skills"][0]["loaded_by"], "skill_load");
    assert_eq!(context["loaded_skills"][0]["trust"], "builtin");
    assert_eq!(context["loaded_skills"][0]["text"], CLEANUP_PROCEDURE);
}

#[test]
fn a_run_without_skills_sends_no_loaded_skills_field() {
    let value = serde_json::to_value(bare_context()).unwrap();
    assert!(value.get("loaded_skills").is_none());
}

#[test]
fn project_skills_follow_the_project_trust() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "rules").unwrap();
    let skills = || {
        vec![
            cleanup_skill(SkillScope::Repo),
            LoadedSkill {
                name: "mine".into(),
                scope: SkillScope::User,
                ..cleanup_skill(SkillScope::User)
            },
        ]
    };
    let names = |trust| {
        let rules = rules_source(project.path(), trust);
        let context = review_context(run_with_skills(skills()), Some(&rules))
            .unwrap()
            .0;
        serde_json::to_value(context).unwrap()["loaded_skills"]
            .as_array()
            .map(|skills| {
                skills
                    .iter()
                    .map(|skill| skill["name"].as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    };
    assert_eq!(names(ProjectTrust::Approved), ["worktree-pr", "mine"]);
    assert_eq!(names(ProjectTrust::Unapproved), ["mine"]);
    // Without any rules source the project is not approved either.
    let without_rules = review_context(run_with_skills(skills()), None).unwrap().0;
    assert_eq!(
        serde_json::to_value(without_rules).unwrap()["loaded_skills"]
            .as_array()
            .map(Vec::len),
        Some(1)
    );
}

#[test]
fn project_rules_are_sent_up_to_sixteen_kib() {
    let project = tempfile::tempdir().unwrap();
    std::fs::write(project.path().join("AGENTS.md"), "r".repeat(20_000)).unwrap();
    let rules = rules_source(project.path(), ProjectTrust::Approved);
    let context = review_context(run_with_skills(vec![]), Some(&rules))
        .unwrap()
        .0;
    assert_eq!(context.project_rules.unwrap().text.len(), 16 * 1024);
}

#[test]
fn a_skill_body_is_cut_at_sixteen_kib_in_the_payload() {
    let mut skill = cleanup_skill(SkillScope::Builtin);
    skill.text = "s".repeat(20_000);
    let context = review_context(run_with_skills(vec![skill]), None)
        .unwrap()
        .0;
    let text = &serde_json::to_value(&context).unwrap()["loaded_skills"][0]["text"];
    let text = text.as_str().unwrap();
    assert!(text.starts_with("sss"));
    assert!(text.ends_with("…[truncated]"));
    assert!(text.len() < 17 * 1024);
}

#[tokio::test]
async fn an_empty_procedure_is_not_a_procedure_source() {
    let verified = response(true, "critical", "high", Some("verified"));
    let empty_rules = |text: &str| {
        let project = tempfile::tempdir().unwrap();
        std::fs::write(project.path().join("AGENTS.md"), text).unwrap();
        let rules = rules_source(project.path(), ProjectTrust::Approved);
        review_context(run_with_skills(vec![]), Some(&rules))
            .unwrap()
            .0
    };
    let empty_skill = |text: &str| {
        let mut skill = cleanup_skill(SkillScope::Builtin);
        skill.text = text.into();
        review_context(run_with_skills(vec![skill]), None)
            .unwrap()
            .0
    };
    for (name, context) in [
        ("empty AGENTS.md", empty_rules("")),
        ("blank AGENTS.md", empty_rules(" \n\t\n")),
        ("empty skill", empty_skill("")),
        ("blank skill", empty_skill("  \n")),
    ] {
        assert_eq!(shell(context, &verified).await, requires_user(), "{name}");
    }
}
