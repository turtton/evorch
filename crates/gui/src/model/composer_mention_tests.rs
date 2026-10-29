use super::*;

fn index(paths: &[&str], skills: &[(&str, &str)]) -> MentionIndex {
    let files: Vec<_> = paths.iter().map(|path| (*path).to_owned()).collect();
    MentionIndex::from_parts(
        None,
        &files,
        skills
            .iter()
            .map(|(name, description)| ((*name).to_owned(), (*description).to_owned())),
    )
}

fn labels(index: &MentionIndex, input: &str) -> Vec<String> {
    let mention = mention_at(input, input.len()).expect("mention");
    index
        .complete(&mention)
        .into_iter()
        .map(|item| item.label)
        .collect()
}

fn write_skill_in(dir: &Path, name: &str, body: &str) {
    std::fs::create_dir_all(dir.join(name)).expect("skill dir");
    std::fs::write(
        dir.join(name).join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {name} skill\n---\n{body}\n"),
    )
    .expect("SKILL.md");
}

fn skill_registry(root: &Path, skills: &[(&str, &str)]) -> SkillRegistry {
    let dir = root.join(".evorch/skills");
    for (name, body) in skills {
        write_skill_in(&dir, name, body);
    }
    runtime::skill::discover_skills(&[(runtime::skill::SkillScope::Repo, dir)])
}

fn git_init(root: &Path) {
    let mut init = std::process::Command::new("git");
    for variable in GIT_LOCAL_ENV {
        init.env_remove(variable);
    }
    let status = init
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .expect("git init");
    assert!(status.success());
}

fn write_files(root: &Path, paths: &[&str]) {
    for path in paths {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("dir");
        std::fs::write(path, "").expect("file");
    }
}

fn base_of(directory: &Path) -> String {
    slash_path(directory.to_str().expect("utf-8 temp path"))
}

fn values(index: &MentionIndex) -> Vec<(CompletionKind, String)> {
    index
        .entries
        .iter()
        .map(|entry| (entry.kind, entry.value.clone()))
        .collect()
}

#[test]
fn mention_token_requires_word_boundary_and_covers_whole_token() {
    for (input, cursor, expected) in [
        ("@", 1, Some(("", 0..1))),
        ("see @src/ma", 11, Some(("src/ma", 4..11))),
        ("see @src/main.rs now", 9, Some(("src/", 4..16))),
        (
            "日本語 @読",
            "日本語 @読".len(),
            Some(("読", "日本語 ".len().."日本語 @読".len())),
        ),
        ("mail a@b.c", 10, None),
        ("@done then", 10, None),
        ("", 0, None),
    ] {
        let actual = mention_at(input, cursor).map(|m| (m.query, m.range));
        assert_eq!(actual, expected, "{input:?} @ {cursor}");
    }
}

#[test]
fn ranking_prefers_name_prefix_then_shallow_paths() {
    let index = index(
        &[
            "crates/gui/src/main.rs",
            "src/main.rs",
            "docs/domain.md",
            "README.md",
        ],
        &[("maintain", "keeps things tidy")],
    );
    assert_eq!(
        labels(&index, "@main"),
        [
            "@maintain",
            "@src/main.rs",
            "@crates/gui/src/main.rs",
            "@docs/domain.md"
        ]
    );
    // An empty query browses the project root, folders and skills included.
    assert_eq!(
        labels(&index, "@"),
        ["@crates/", "@docs/", "@maintain", "@README.md", "@src/"]
    );
    // A path query lists a folder's direct children before deeper descendants.
    assert_eq!(
        labels(&index, "@crates/gui/"),
        ["@crates/gui/src/", "@crates/gui/src/main.rs"]
    );
    assert!(labels(&index, "@zzz").is_empty());
}

#[test]
fn accepting_a_folder_keeps_browsing_while_files_and_skills_end_the_token() {
    let index = index(&["src/main.rs"], &[("review", "reviews")]);
    let input = "look at @sr please";
    let mention = mention_at(input, "look at @sr".len()).expect("mention");
    let items = index.complete(&mention);
    let dir = items
        .iter()
        .find(|item| item.kind == CompletionKind::Dir)
        .expect("dir");
    let file = items
        .iter()
        .find(|item| item.kind == CompletionKind::File)
        .expect("file");
    assert_eq!(
        (dir.replacement.as_str(), dir.range.clone()),
        ("@src/", 8..11)
    );
    assert_eq!(file.replacement, "@src/main.rs ");
    let skill = &index.complete(&mention_at("@rev", 4).expect("mention"))[0];
    assert_eq!(skill.replacement, "@review ");
    // A token that already spells the candidate is left for Enter to send.
    let exact = &index.complete(&mention_at("@review", 7).expect("mention"))[0];
    assert!(!exact.changes("@review"));
    assert!(skill.changes("@rev"));
    assert!(dir.changes(input));
}

#[test]
fn completions_are_capped() {
    let paths: Vec<_> = (0..20).map(|i| format!("file{i:02}.rs")).collect();
    let paths: Vec<_> = paths.iter().map(String::as_str).collect();
    assert_eq!(labels(&index(&paths, &[]), "@file").len(), MAX_COMPLETIONS);
}

#[test]
fn skill_mentions_name_each_skill_once_and_leave_folders_and_unknown_names_as_text() {
    let temp = tempfile::tempdir().expect("root");
    let skills = skill_registry(
        temp.path(),
        &[("review", "review body"), ("docs", "docs body")],
    );
    std::fs::create_dir(temp.path().join("docs")).expect("docs folder");
    let typed = "use @review, then @review again on @docs and @unknown";

    // A project folder named like a skill stays a path; the body is the runtime's to read.
    assert_eq!(
        mentioned_skills(typed, Some(temp.path()), &skills),
        ["review"]
    );
    assert!(mentioned_skills("plain text", Some(temp.path()), &skills).is_empty());
}

#[test]
fn untrusted_mentions_inline_user_and_builtin_skills_and_name_withheld_repo_ones() {
    use runtime::skill::{SkillScope, discover_with_builtin};
    // Given: repository skills, one shadowing a user skill and one named like
    // a project folder, beside a user skill and the builtin ones.
    let project = tempfile::tempdir().expect("root");
    let user = tempfile::tempdir().expect("user");
    let root = project.path();
    let repo = root.join(".evorch/skills");
    for (name, body) in [
        ("repo-only", "repo only body"),
        ("shared", "repo shared body"),
        ("docs", "repo docs body"),
    ] {
        write_skill_in(&repo, name, body);
    }
    for (name, body) in [("shared", "user shared body"), ("user-only", "user body")] {
        write_skill_in(user.path(), name, body);
    }
    std::fs::create_dir(root.join("docs")).expect("docs folder");
    let user_scope = (SkillScope::User, user.path().to_owned());
    let untrusted = discover_with_builtin(std::slice::from_ref(&user_scope));
    let trusted = discover_with_builtin(&[(SkillScope::Repo, repo), user_scope]);
    let typed = "@repo-only @shared @user-only @git-best-practices @docs @unknown @repo-only";

    // When: an untrusted project's skills are attached for the mentions.
    let attached = mentioned_skills(typed, Some(root), &untrusted);

    // Then: user and builtin skills are named, the user `shared` rather than
    // the repository one it would lose to in a trusted project.
    assert_eq!(attached, ["shared", "user-only", "git-best-practices"]);
    // Then: only the repository skill a trusted project would inline instead
    // is named, once; the folder stays a path and the unknown name text.
    assert_eq!(
        withheld_skill_mentions(typed, Some(root), &untrusted, &trusted),
        ["repo-only"]
    );
    // Then: a trusted project withholds nothing.
    assert!(withheld_skill_mentions(typed, Some(root), &trusted, &trusted).is_empty());
}

#[test]
fn git_index_honours_gitignore_and_lists_untracked_files() {
    let temp = tempfile::tempdir().expect("root");
    let root = temp.path();
    git_init(root);
    std::fs::create_dir_all(root.join("src")).expect("src");
    std::fs::create_dir_all(root.join("build")).expect("build");
    std::fs::write(root.join(".gitignore"), "build/\n").expect("ignore");
    std::fs::write(root.join("src/lib.rs"), "").expect("lib");
    std::fs::write(root.join("build/out.bin"), "").expect("out");

    let index = MentionIndex::build(root, &[], &skill_registry(root, &[("lint", "body")]), true);

    let values: Vec<_> = index
        .entries
        .iter()
        .map(|entry| (entry.kind, entry.value.as_str()))
        .collect();
    assert_eq!(
        values,
        [
            (CompletionKind::Dir, ".evorch"),
            (CompletionKind::Dir, ".evorch/skills"),
            (CompletionKind::Dir, ".evorch/skills/lint"),
            (CompletionKind::Dir, "src"),
            (CompletionKind::File, ".evorch/skills/lint/SKILL.md"),
            (CompletionKind::File, ".gitignore"),
            (CompletionKind::File, "src/lib.rs"),
            (CompletionKind::Skill, "lint"),
        ]
    );
}

#[test]
fn non_git_index_walks_the_tree_skipping_build_output() {
    let temp = tempfile::tempdir().expect("root");
    let root = temp.path();
    for path in ["src/lib.rs", "target/debug/app", "node_modules/x/index.js"] {
        std::fs::create_dir_all(root.join(path).parent().expect("parent")).expect("dir");
        std::fs::write(root.join(path), "").expect("file");
    }
    let index = MentionIndex::build(root, &[], &skill_registry(root, &[]), true);
    let values: Vec<_> = index
        .entries
        .iter()
        .map(|entry| entry.value.as_str())
        .collect();
    assert_eq!(values, ["src", "src/lib.rs"]);
}

#[test]
fn allowed_directories_complete_to_absolute_paths() {
    // Given: a project file plus an allowed directory outside the project.
    let mut index = index(&["src/main.rs"], &[("share", "skill")]);
    index.add_allowed_directory(
        Path::new("/work/shared-lib"),
        &["src/util.rs".into(), "README.md".into()],
    );

    // Then: an empty query lists the allowed directory next to the project root entries.
    assert_eq!(
        labels(&index, "@"),
        ["@/work/shared-lib/", "@share", "@src/"]
    );
    // Then: its folder name or a file name finds entries, inserted as absolute paths.
    assert_eq!(
        labels(&index, "@shared"),
        [
            "@/work/shared-lib/",
            "@/work/shared-lib/src/",
            "@/work/shared-lib/README.md",
            "@/work/shared-lib/src/util.rs"
        ]
    );
    let util = &index.complete(&mention_at("@util", 5).expect("mention"))[0];
    assert_eq!(
        (util.replacement.as_str(), util.detail.as_str()),
        ("@/work/shared-lib/src/util.rs ", "allowed file")
    );
    // Then: an accepted folder keeps browsing by its absolute path, direct children first.
    assert_eq!(
        labels(&index, "@/work/shared-lib/"),
        [
            "@/work/shared-lib/src/",
            "@/work/shared-lib/README.md",
            "@/work/shared-lib/src/util.rs"
        ]
    );
    // Then: the absolute prefix alone does not make every allowed file match.
    assert_eq!(labels(&index, "@work"), Vec::<String>::new());
}

#[test]
fn build_shares_one_path_budget_with_allowed_directories() {
    // Given: two project files, and three files beside build output in an allowed directory.
    let project = tempfile::tempdir().expect("project");
    let shared = tempfile::tempdir().expect("shared");
    write_files(project.path(), &["src/lib.rs", "src/main.rs"]);
    write_files(
        shared.path(),
        &["docs/a.md", "docs/b.md", "docs/c.md", "target/out"],
    );
    let skills = skill_registry(project.path(), &[]);
    let allowed = [shared.path().to_owned()];
    let base = base_of(shared.path());
    let build = |limit| MentionIndex::build_within(project.path(), &allowed, &skills, true, limit);
    let files = |index: &MentionIndex| -> Vec<String> {
        index
            .entries
            .iter()
            .filter(|entry| entry.kind == CompletionKind::File)
            .map(|entry| entry.value.clone())
            .collect()
    };

    // When: the limit fits all five files exactly.
    let index = build(5);
    // Then: each is indexed once, project paths first, and build output is skipped.
    assert_eq!(
        values(&index),
        [
            (CompletionKind::Dir, "src".to_owned()),
            (CompletionKind::File, "src/lib.rs".to_owned()),
            (CompletionKind::File, "src/main.rs".to_owned()),
            (CompletionKind::Dir, base.clone()),
            (CompletionKind::Dir, format!("{base}/docs")),
            (CompletionKind::File, format!("{base}/docs/a.md")),
            (CompletionKind::File, format!("{base}/docs/b.md")),
            (CompletionKind::File, format!("{base}/docs/c.md")),
        ]
    );
    assert_eq!(index.allowed, allowed);

    // When: the limit is one short.
    let listed = files(&build(4));
    // Then: the allowed directory gets only what the project left.
    assert_eq!(listed.len(), 4);
    assert_eq!(listed[..2], ["src/lib.rs", "src/main.rs"]);
    assert!(
        listed[2..]
            .iter()
            .all(|file| file.starts_with(&format!("{base}/docs/")))
    );

    // When: the project takes the whole limit, or more.
    for limit in [2, 1] {
        let index = build(limit);
        // Then: no allowed file fits, yet the allowed folder itself can still be mentioned.
        let listed = files(&index);
        assert_eq!(listed.len(), limit);
        assert!(listed.iter().all(|file| file.starts_with("src/")));
        assert!(
            index
                .entries
                .iter()
                .any(|entry| entry.kind == CompletionKind::Dir && entry.value == base)
        );
    }
}

#[test]
fn allowed_directory_containing_the_project_leaves_project_files_relative() {
    for git in [false, true] {
        // Given: an allowed directory whose subfolder is the project root.
        let temp = tempfile::tempdir().expect("temp");
        let shared = temp.path().join("shared");
        let project = shared.join("app");
        write_files(&shared, &["app/src/lib.rs", "notes.md"]);
        if git {
            git_init(&shared);
        }
        let base = base_of(&shared);

        // When: the limit fits the two distinct files exactly.
        let index = MentionIndex::build_within(
            &project,
            std::slice::from_ref(&shared),
            &skill_registry(&project, &[]),
            true,
            2,
        );

        // Then: project files stay relative, neither repeated nor charged under the allowed directory.
        assert_eq!(
            values(&index),
            [
                (CompletionKind::Dir, "src".to_owned()),
                (CompletionKind::File, "src/lib.rs".to_owned()),
                (CompletionKind::Dir, base.clone()),
                (CompletionKind::File, format!("{base}/notes.md")),
            ],
            "git: {git}"
        );
    }
}

#[test]
fn nested_duplicate_and_missing_allowed_directories_add_each_path_once() {
    // Given: an allowed directory added before its parent, a duplicate, and a missing one.
    let project = tempfile::tempdir().expect("project");
    let temp = tempfile::tempdir().expect("temp");
    let outer = temp.path().join("outer");
    let inner = outer.join("inner");
    write_files(project.path(), &["src/lib.rs"]);
    write_files(&outer, &["inner/deep.md", "top.md"]);
    let allowed = [
        inner,
        temp.path().join("missing"),
        outer.clone(),
        outer.clone(),
    ];
    let base = base_of(&outer);

    // When: the limit fits the three distinct files exactly.
    let index = MentionIndex::build_within(
        project.path(),
        &allowed,
        &skill_registry(project.path(), &[]),
        true,
        3,
    );

    // Then: the nested directory lists its own files, which the parent skips,
    // so no path is listed twice.
    assert_eq!(
        values(&index),
        [
            (CompletionKind::Dir, "src".to_owned()),
            (CompletionKind::File, "src/lib.rs".to_owned()),
            (CompletionKind::Dir, format!("{base}/inner")),
            (CompletionKind::File, format!("{base}/inner/deep.md")),
            (CompletionKind::Dir, base.clone()),
            (CompletionKind::File, format!("{base}/top.md")),
        ]
    );
    // Then: the nested folder is found by its name, and browsed like any allowed directory.
    assert_eq!(labels(&index, "@inner")[0], format!("@{base}/inner/"));
    assert!(labels(&index, "@").contains(&format!("@{base}/inner/")));
    // Then: the index keeps the list it was built for, so that list does not look stale.
    assert_eq!(index.allowed, allowed);
    assert!(index.is_built_for(project.path(), &allowed, true));
}

#[test]
fn a_nested_allowed_directory_completes_where_its_parent_lists_none_of_it() {
    // Each way the parent's listing leaves the nested directory out.
    for case in ["ignored by git", "own repository", "skipped folder"] {
        // Given: a nested allowed directory with a file two levels down.
        let project = tempfile::tempdir().expect("project");
        let temp = tempfile::tempdir().expect("temp");
        let outer = temp.path().join("outer");
        let inner = outer.join(if case == "skipped folder" {
            "target"
        } else {
            "inner"
        });
        write_files(project.path(), &["src/lib.rs"]);
        write_files(&outer, &["top.md"]);
        write_files(&inner, &["sub/deep.md"]);
        match case {
            "ignored by git" => {
                git_init(&outer);
                std::fs::write(outer.join(".gitignore"), "inner/\n").expect("ignore");
            }
            "own repository" => {
                git_init(&outer);
                git_init(&inner);
            }
            _ => {}
        }
        let skills = skill_registry(project.path(), &[]);
        let deep = format!("@{}/sub/deep.md ", base_of(&inner));
        let top = format!("@{}/top.md ", base_of(&outer));

        // When: it is approved alone, then with its parent added after it (or listed before it).
        for allowed in [
            vec![inner.clone()],
            vec![inner.clone(), outer.clone()],
            vec![outer.clone(), inner.clone()],
        ] {
            let index = MentionIndex::build(project.path(), &allowed, &skills, true);
            let first = |query: &str| {
                index
                    .complete(&mention_at(query, query.len()).expect("mention"))
                    .first()
                    .map(|item| item.replacement.clone())
            };

            // Then: its file is found by name and inserted as an absolute path.
            assert_eq!(first("@deep").as_ref(), Some(&deep), "{case}: {allowed:?}");
            // Then: the parent's own files complete too, and no path is listed twice.
            if allowed.contains(&outer) {
                assert_eq!(first("@top").as_ref(), Some(&top), "{case}: {allowed:?}");
            }
            let listed = values(&index);
            let distinct: BTreeSet<_> = listed.iter().map(|(_, value)| value).collect();
            assert_eq!(distinct.len(), listed.len(), "{case}: {allowed:?}");
        }
    }
}

#[test]
fn only_allowed_directories_are_walked_when_their_repository_ignores_them() {
    // Given: a repository that ignores a folder holding a file.
    let temp = tempfile::tempdir().expect("temp");
    let repo = temp.path().join("repo");
    let ignored = repo.join("ignored");
    write_files(&ignored, &["notes.md"]);
    git_init(&repo);
    std::fs::write(repo.join(".gitignore"), "ignored/\n").expect("ignore");
    let elsewhere = tempfile::tempdir().expect("project");
    write_files(elsewhere.path(), &["src/lib.rs"]);

    // When: the folder is the project root, it keeps git's listing, which is empty.
    let project = MentionIndex::build(&ignored, &[], &skill_registry(&ignored, &[]), true);
    assert!(values(&project).is_empty());

    // When: the folder is approved as an allowed directory, it is walked instead.
    let approved = MentionIndex::build(
        elsewhere.path(),
        std::slice::from_ref(&ignored),
        &skill_registry(elsewhere.path(), &[]),
        true,
    );
    assert!(values(&approved).contains(&(
        CompletionKind::File,
        format!("{}/notes.md", base_of(&ignored))
    )));
}

#[test]
fn withholding_repo_skills_drops_every_skill_until_a_rebuild() {
    // Given: an index built without the project's repository skills.
    let root = Path::new("/work/app");
    let mut index = index(&["src/main.rs"], &[("share", "skill")]);
    index.root = Some(root.to_owned());
    let files = [
        (CompletionKind::Dir, "src".to_owned()),
        (CompletionKind::File, "src/main.rs".to_owned()),
    ];
    // When: they are withheld, there is nothing of theirs to drop.
    index.withhold_repo_skills();
    // Then: it keeps its skills and stays built for an untrusted project only.
    assert_eq!(
        values(&index)[files.len()..],
        [(CompletionKind::Skill, "share".to_owned())]
    );
    assert!(index.is_built_for(root, &[], false));
    assert!(!index.is_built_for(root, &[], true));

    // Given: the same index built with the project's repository skills.
    index.repo_skills = true;
    assert!(index.is_built_for(root, &[], true));
    // When: the project stops trusting them.
    index.withhold_repo_skills();
    // Then: every skill goes at once while files stay, and the index is due a
    // rebuild whichever trust the project ends up with.
    assert_eq!(values(&index), files);
    assert!(labels(&index, "@share").is_empty());
    assert!(!index.is_built_for(root, &[], false));
    assert!(!index.is_built_for(root, &[], true));
    // Then: withholding again changes nothing.
    let before = index.clone();
    index.withhold_repo_skills();
    assert_eq!(index, before);
}

#[test]
fn retain_allowed_drops_only_entries_outside_the_kept_directories() {
    // Given: project entries plus two allowed directories, one named like a prefix of the other.
    let root = Path::new("/work/app");
    let mut index = index(&["src/main.rs"], &[("share", "skill")]);
    index.add_allowed_directory(Path::new("/work/lib"), &["a.rs".into()]);
    index.add_allowed_directory(Path::new("/work/lib-old"), &["b.rs".into()]);
    let built: Vec<PathBuf> = vec!["/work/lib".into(), "/work/lib-old".into()];
    index.root = Some(root.to_owned());
    index.allowed = built.clone();
    index.retained = built.clone();
    let project = [
        (CompletionKind::Dir, "src".to_owned()),
        (CompletionKind::File, "src/main.rs".to_owned()),
        (CompletionKind::Skill, "share".to_owned()),
    ];
    assert!(index.is_built_for(root, &built, false));
    assert!(!labels(&index, "@lib-old").is_empty());

    // When: `/work/lib-old` is revoked as another directory is approved.
    let current: Vec<PathBuf> = vec!["/work/new".into(), "/work/lib".into()];
    index.retain_allowed(&current);
    // Then: only the revoked entries go; the new directory waits for a rebuild.
    assert_eq!(index.retained, [PathBuf::from("/work/lib")]);
    assert_eq!(
        values(&index)[project.len()..],
        [
            (CompletionKind::Dir, "/work/lib".to_owned()),
            (CompletionKind::File, "/work/lib/a.rs".to_owned()),
        ]
    );
    assert!(labels(&index, "@lib-old").is_empty());
    // Then: the index still names the list it was built for, so it is due a
    // rebuild for the current list, the kept part alone, or the original list
    // if the revoked directory is approved again before a rebuild lands.
    assert_eq!(index.allowed, built);
    assert!(!index.is_built_for(root, &built, false));
    assert!(!index.is_built_for(root, &current, false));
    assert!(!index.is_built_for(root, &["/work/lib".into()], false));
    // Then: dropping nothing more leaves it as it is.
    let before = index.clone();
    index.retain_allowed(&current);
    assert_eq!(index, before);

    // When: nothing stays approved.
    index.retain_allowed(&[]);
    // Then: project entries and skills remain.
    assert!(index.retained.is_empty());
    assert_eq!(values(&index), project);
    assert!(!index.is_built_for(root, &[], false));

    // Given: a parent and the nested allowed directory it leaves to list its own files.
    let mut nested = MentionIndex::default();
    nested.add_allowed_directory(Path::new("/work/outer"), &["top.md".into()]);
    nested.add_allowed_directory(Path::new("/work/outer/inner"), &["deep.md".into()]);
    nested.allowed = vec!["/work/outer/inner".into(), "/work/outer".into()];
    nested.retained = nested.allowed.clone();
    // When: the parent is revoked but the nested directory stays.
    nested.retain_allowed(&["/work/outer/inner".into()]);
    // Then: the nested directory keeps its entries, named as if listed alone.
    let mut alone = MentionIndex::default();
    alone.add_allowed_directory(Path::new("/work/outer/inner"), &["deep.md".into()]);
    assert_eq!(nested.entries, alone.entries);
    assert_eq!(labels(&nested, "@"), ["@/work/outer/inner/"]);
}
