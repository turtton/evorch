//! T7: skill レジストリのランタイム配線の結合テスト (issue #53)。
//!
//! `with_skills` が初回接続時にレジストリの診断 1 件ごとに
//! [`FaultEvent::SkillDiagnostic`] を 1 件発行すること (ADR 0010)、および
//! 先勝ちにより 2 回目の接続で診断が重複発行されないことを
//! ランタイムの実イベントバス上で検証する。

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use event_bus::{EventBus, EventKind, EventReceiver, FaultEvent, SkillDiagnosticKind};
use runtime::AgentRuntime;
use runtime::skill::{SkillRegistry, SkillScope, discover_skills};
use sandbox::DirectSandbox;
use tempfile::tempdir;
use tokio::time::{Duration, timeout};
use tools::ToolExecutor;

use support::ScriptedModel;

/// 診断 1 件に対応するイベントペイロードの要約。
type DiagnosticRecord = (SkillDiagnosticKind, String, String, String);

/// name/description/body から SKILL.md を持つ skill ディレクトリを作る。
fn write_skill(dir: &Path, name: &str, description: &str, body: &str) {
    let skill_dir = dir.join(name);
    fs::create_dir_all(&skill_dir).unwrap();
    fs::write(
        skill_dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n{body}"),
    )
    .unwrap();
}

fn runtime_with(bus: Arc<EventBus>) -> AgentRuntime {
    let executor = Arc::new(ToolExecutor::with_standard_tools(
        Arc::clone(&bus),
        Arc::new(DirectSandbox::new_unchecked()),
    ));
    AgentRuntime::new(bus, executor, Arc::new(ScriptedModel::new([])))
}

/// Shadowed と ValidationError の診断を 2 件持つレジストリを組み立てる。
///
/// repo スコープに有効な `demo-skill` と不正な `broken-skill`、user スコープに
/// 同名の `demo-skill` を置く。発見順は repo → user なので診断は
/// [ValidationError, Shadowed] の順で 2 件積まれる。
fn registry_with_two_diagnostics() -> (SkillRegistry, tempfile::TempDir, tempfile::TempDir) {
    let repo_root = tempdir().unwrap();
    let user_root = tempdir().unwrap();
    let repo_skills = repo_root.path().join("skills");
    let user_skills = user_root.path().join("skills");
    write_skill(&repo_skills, "demo-skill", "Repo version", "Repo body.\n");
    write_skill(&user_skills, "demo-skill", "User version", "User body.\n");
    let broken = repo_skills.join("broken-skill");
    fs::create_dir_all(&broken).unwrap();
    fs::write(
        broken.join("SKILL.md"),
        "---\nname: other-name\ndescription: Broken skill\n---\nBody.\n",
    )
    .unwrap();

    let registry = discover_skills(&[
        (SkillScope::Repo, repo_skills),
        (SkillScope::User, user_skills),
    ]);
    (registry, repo_root, user_root)
}

/// バスを購読して SkillDiagnostic fault だけを収集する。
///
/// emit は `with_skills` 内で同期的に完了しているため、バッファ消化後の
/// 最初の recv タイムアウトで打ち切れば決定的に取りこぼしなしで読める。
async fn drain_skill_diagnostics(receiver: &mut EventReceiver) -> Vec<DiagnosticRecord> {
    let mut found = Vec::new();
    while let Ok(Ok(event)) = timeout(Duration::from_millis(100), receiver.recv()).await {
        if let EventKind::Fault(FaultEvent::SkillDiagnostic {
            kind,
            skill,
            scope,
            detail,
        }) = event.kind
        {
            found.push((kind, skill, scope, detail));
        }
    }
    found
}

// Given: Shadowed と ValidationError の診断を 2 件持つレジストリと、with_skills 前に
//        購読した受信者
// When: with_skills でレジストリを接続する
// Then: 診断 1 件ごとに FaultEvent::SkillDiagnostic が 1 件、診断順に発行される
#[tokio::test]
async fn with_skills_emits_one_fault_event_per_diagnostic() {
    let (registry, _repo_root, _user_root) = registry_with_two_diagnostics();
    assert_eq!(registry.diagnostics.len(), 2);

    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let _runtime = runtime_with(Arc::clone(&bus)).with_skills(Arc::new(registry));

    let records = drain_skill_diagnostics(&mut receiver).await;

    assert_eq!(records.len(), 2);
    let (kind, skill, scope, detail) = &records[0];
    assert!(matches!(kind, SkillDiagnosticKind::ValidationError));
    assert_eq!(skill, "other-name");
    assert_eq!(scope, "repo");
    assert!(!detail.is_empty());
    let (kind, skill, scope, detail) = &records[1];
    assert!(matches!(kind, SkillDiagnosticKind::Shadowed));
    assert_eq!(skill, "demo-skill");
    assert_eq!(scope, "user");
    assert!(!detail.is_empty());
}

// Given: 診断 2 件のレジストリと with_skills 前に購読した受信者
// When: 診断を持つ別レジストリで with_skills を 2 回呼ぶ
// Then: 診断は 2 件のまま (先勝ちで 2 回目は無視され、重複発行されない)
#[tokio::test]
async fn with_skills_twice_does_not_duplicate_fault_emission() {
    let (first, _repo_root, _user_root) = registry_with_two_diagnostics();
    let (second, _repo_root2, _user_root2) = registry_with_two_diagnostics();

    let bus = Arc::new(EventBus::new(64));
    let mut receiver = bus.subscribe();
    let _runtime = runtime_with(Arc::clone(&bus))
        .with_skills(Arc::new(first))
        .with_skills(Arc::new(second));

    let records = drain_skill_diagnostics(&mut receiver).await;

    assert_eq!(records.len(), 2);
}
