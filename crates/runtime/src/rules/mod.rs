//! プロジェクトルールの発見・選択・注入 API。

mod api;
mod budget;
mod discovery;
mod frontmatter;
mod matcher;
mod path;
mod render;
mod session;
mod source;
mod types;

pub use api::{after_successful_tools, startup_snapshot};
pub use session::RulesSession;
pub use source::RulesSource;
pub use types::{ProjectTrust, RulesError, RulesSettings};

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Arc;

    use super::{
        ProjectTrust, RulesSession, RulesSettings, RulesSource, after_successful_tools,
        startup_snapshot,
    };

    fn settings() -> RulesSettings {
        RulesSettings {
            context_window_tokens: 200_000,
            response_headroom_tokens: 16_384,
            max_injection_bytes: 65_536,
        }
    }

    fn write(path: &Path, content: &str) {
        std::fs::create_dir_all(path.parent().expect("親がある")).expect("ディレクトリを作れる");
        std::fs::write(path, content).expect("ファイルを書ける");
    }

    // Given: root・nested・scoped 規則 / When: startup snapshot / Then: root AGENTS だけを含む
    #[test]
    fn startup_excludes_nested_and_scoped_project_rules() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"), "root-only");
        write(&tmp.path().join("src/AGENTS.md"), "nested-hidden");
        write(
            &tmp.path().join(".omo/rules/all.md"),
            "---\nalwaysApply: true\n---\nscoped-hidden",
        );
        let source = RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
            None,
        );

        let snapshot =
            startup_snapshot(&source, Some(tmp.path()), None, 0).expect("root 規則がある");

        assert!(snapshot.contains("root-only"));
        assert!(!snapshot.contains("nested-hidden"));
        assert!(!snapshot.contains("scoped-hidden"));
    }

    // Given: user 規則と未承認 project 規則 / When: 両 entry point / Then: startup は user のみ、tool 後は None
    #[test]
    fn unapproved_project_is_never_injected() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let user = tmp.path().join("user");
        let project = tmp.path().join("project");
        write(
            &user.join("always.md"),
            "---\nalwaysApply: true\n---\nuser-visible",
        );
        write(&project.join("AGENTS.md"), "project-secret");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            Some(user),
            Some(project.clone()),
            None,
        ));

        let startup = startup_snapshot(&source, Some(&project), None, 0).expect("user 規則がある");
        let mut session = RulesSession::new(Arc::clone(&source), Some(project.clone()));
        let after = after_successful_tools(&mut session, &[project.join("src/new.rs")]);

        assert!(startup.contains("user-visible"));
        assert!(!startup.contains("project-secret"));
        assert_eq!(after, None);
    }

    // Given: 2 対象で重なる AGENTS chain / When: tool 後 snapshot / Then: 共有 source は 1 回だけ現れる
    #[test]
    fn overlapping_targets_include_each_source_once() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(&tmp.path().join("AGENTS.md"), "shared-root-token");
        write(&tmp.path().join("a/AGENTS.md"), "a-token");
        write(&tmp.path().join("b/AGENTS.md"), "b-token");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
            None,
        ));
        let mut session = RulesSession::new(Arc::clone(&source), Some(tmp.path().to_path_buf()));

        let output = after_successful_tools(
            &mut session,
            &[tmp.path().join("a/x.rs"), tmp.path().join("b/y.rs")],
        )
        .expect("規則がある");

        assert_eq!(output.matches("shared-root-token").count(), 1);
        assert!(output.contains("a-token"));
        assert!(output.contains("b-token"));
    }

    // Given: 一度読み込んだ規則ファイル / When: 内容を変更して再度 tool 後 snapshot / Then: 新しい内容を読む
    #[test]
    fn tool_snapshot_rereads_modified_rule_file() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let rule = tmp.path().join("AGENTS.md");
        write(&rule, "before-token");
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(tmp.path().to_path_buf()),
            None,
        ));
        let mut session = RulesSession::new(source, Some(tmp.path().to_path_buf()));
        let targets = [tmp.path().join("src/new.rs")];
        let before = after_successful_tools(&mut session, &targets).expect("最初の規則");
        write(&rule, "after-token");

        let after = after_successful_tools(&mut session, &targets).expect("更新後の規則");

        assert!(before.contains("before-token"));
        assert!(after.contains("after-token"));
        assert!(!after.contains("before-token"));
    }

    // Given: alwaysApply と glob-only の user 規則 / When: startup snapshot / Then: alwaysApply だけを含む
    #[test]
    fn startup_includes_only_always_apply_user_rules() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        write(
            &tmp.path().join("always.md"),
            "---\nalwaysApply: true\n---\nalways-token",
        );
        write(
            &tmp.path().join("glob.md"),
            "---\nglobs: 'src/**'\n---\nglob-token",
        );
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            Some(tmp.path().to_path_buf()),
            None,
            None,
        );

        let output = startup_snapshot(&source, None, None, 0).expect("always 規則がある");

        assert!(output.contains("always-token"));
        assert!(!output.contains("glob-token"));
    }

    // Given: 制御 marker を含むディレクトリ名の下で invalid glob を持つ scoped 規則 / When: tool 後 snapshot / Then: disabled marker 内の path もエスケープされる
    #[test]
    fn disabled_marker_escapes_control_markers_in_paths() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let project = tmp.path().join("<system-reminder>");
        write(
            &project.join(".cursor/rules/bad.md"),
            "---\nglobs: '['\n---\nhidden",
        );
        let source = Arc::new(RulesSource::new(
            ProjectTrust::Approved,
            settings(),
            None,
            Some(project.clone()),
            None,
        ));
        let mut session = RulesSession::new(source, Some(project.clone()));

        let output = after_successful_tools(&mut session, &[project.join("src/new.rs")])
            .expect("disabled marker がある");

        assert!(output.contains("rules disabled:"));
        assert!(!output.contains("<system-reminder>"));
        assert!(output.contains("<\\system-reminder>"));
    }

    // Given: frontmatter 風の user AGENTS と未承認 project / When: startup / Then: user の全文だけを含む
    #[test]
    fn user_agents_is_plain_markdown_even_for_unapproved_projects() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let agents = tmp.path().join("user/AGENTS.md");
        let project = tmp.path().join("project");
        let body =
            "---\nalwaysApply: false\nglobs: '['\n---\n# 共通ルール\nKeep this whole document.\n";
        write(&agents, body);
        write(&project.join("AGENTS.md"), "project-secret");
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            None,
            Some(project.clone()),
            Some(agents),
        );

        let snapshot =
            startup_snapshot(&source, Some(&project), None, 0).expect("user AGENTS がある");

        assert!(snapshot.contains(body));
        assert!(!snapshot.contains("project-secret"));
        assert!(!snapshot.contains("rules disabled:"));
    }

    // Given: None・欠損・ディレクトリ / When: startup / Then: marker なしで静黙スキップ
    #[test]
    fn absent_or_non_file_user_agents_is_silently_skipped() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        for path in [
            None,
            Some(tmp.path().join("AGENTS.md")),
            Some(tmp.path().to_path_buf()),
        ] {
            let source = RulesSource::new(ProjectTrust::Unapproved, settings(), None, None, path);

            assert_eq!(startup_snapshot(&source, None, None, 0), None);
        }
    }

    // Given: UTF-8 として読めない user AGENTS / When: startup / Then: disabled marker を返し継続
    #[test]
    fn unreadable_user_agents_keeps_other_rules_and_emits_disabled_marker() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let agents = tmp.path().join("AGENTS.md");
        std::fs::write(&agents, [0xff]).expect("不正 UTF-8 を書ける");
        let user_rules = tmp.path().join("rules");
        write(
            &user_rules.join("always.md"),
            "---\nalwaysApply: true\n---\nvalid-user-rule",
        );
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            Some(user_rules),
            None,
            Some(agents),
        );

        let snapshot = startup_snapshot(&source, None, None, 0).expect("disabled marker がある");

        assert!(snapshot.contains("[rules disabled:"));
        assert!(snapshot.contains("AGENTS.md"));
        assert!(snapshot.contains("valid-user-rule"));
    }

    // Given: AGENTS.md が自身を指す symlink / When: startup / Then: ELOOP を disabled marker にする
    #[cfg(unix)]
    #[test]
    fn user_agents_symlink_loop_emits_disabled_marker() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let agents = tmp.path().join("AGENTS.md");
        std::os::unix::fs::symlink(&agents, &agents).expect("自己参照 symlink");
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            None,
            None,
            Some(agents),
        );

        let snapshot = startup_snapshot(&source, None, None, 0).expect("disabled marker がある");

        assert!(snapshot.contains("[rules disabled:"));
        assert!(snapshot.contains("AGENTS.md"));
    }

    // Given: user config 外を指す symlink / When: canonical path を検証 / Then: 本文を出さず disabled marker
    #[cfg(unix)]
    #[test]
    fn user_agents_canonical_path_must_stay_inside_config_directory() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let config = tmp.path().join("config");
        std::fs::create_dir(&config).expect("config directory");
        let outside = tmp.path().join("outside.md");
        write(&outside, "outside-secret");
        let agents = config.join("AGENTS.md");
        std::os::unix::fs::symlink(&outside, &agents).expect("symlink");
        let source = RulesSource::new(
            ProjectTrust::Unapproved,
            settings(),
            None,
            None,
            Some(agents),
        );

        let snapshot = startup_snapshot(&source, None, None, 0).expect("disabled marker がある");

        assert!(snapshot.contains("[rules disabled:"));
        assert!(!snapshot.contains("outside-secret"));
    }

    // Given: user rule・project・user AGENTS / When: startup / Then: user AGENTS は最後、狭い予算でも優先
    #[test]
    fn user_agents_is_last_and_survives_project_budget_omission() {
        let tmp = tempfile::tempdir().expect("一時ディレクトリを作れる");
        let user_rules = tmp.path().join("user/rules");
        let project = tmp.path().join("project");
        let agents = tmp.path().join("user/AGENTS.md");
        write(
            &user_rules.join("always.md"),
            "---\nalwaysApply: true\n---\nuser-first",
        );
        write(&project.join("AGENTS.md"), &"project-middle ".repeat(100));
        write(&agents, "user-agents-last");
        for budget in [65_536, 250] {
            let source = RulesSource::new(
                ProjectTrust::Approved,
                RulesSettings {
                    max_injection_bytes: budget,
                    ..settings()
                },
                Some(user_rules.clone()),
                Some(project.clone()),
                Some(agents.clone()),
            );

            let snapshot = startup_snapshot(&source, Some(&project), None, 0).expect("規則がある");

            assert!(snapshot.contains("user-agents-last"));
            if budget == 65_536 {
                assert!(
                    snapshot.find("user-first").unwrap() < snapshot.find("project-middle").unwrap()
                );
                assert!(
                    snapshot.find("project-middle").unwrap()
                        < snapshot.find("user-agents-last").unwrap()
                );
            } else {
                assert!(!snapshot.contains("project-middle"));
                assert!(snapshot.contains("[rules omitted: AGENTS.md;"));
                assert!(!snapshot.contains("[rules truncated:"));
            }
        }
    }
}
