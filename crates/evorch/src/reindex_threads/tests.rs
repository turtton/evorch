use super::*;

fn parse(tail: &[&str]) -> Result<ReindexArgs, String> {
    parse_args(
        std::iter::once("reindex-threads")
            .chain(tail.iter().copied())
            .map(str::to_owned),
    )
}

#[test]
fn parse_filters_and_reject_bad_boundaries() {
    let args = parse(&[
        "--user-config",
        "config",
        "--project",
        ".",
        "--since",
        "0",
        "--include-evicted",
        "--wait",
    ])
    .unwrap();
    assert_eq!(args.user_config_dir, Some("config".into()));
    assert_eq!(args.project, Some(".".into()));
    assert_eq!(args.since_ns, Some(0));
    assert!(args.include_evicted && args.wait);
    for tail in [
        vec!["--since", "-1"],
        vec!["--since", "9223372036854775808"],
        vec!["--since", "2026-10-25"],
        vec!["--project"],
        vec!["--user-config", "--wait"],
        vec!["--wait", "--wait"],
        vec!["--unknown"],
    ] {
        assert!(parse(&tail).is_err(), "{tail:?}");
    }
    assert_eq!(
        parse(&["--since", "9223372036854775807"]).unwrap().since_ns,
        Some(i64::MAX)
    );
}

#[test]
fn db_is_refused_for_user_config() {
    for tail in [vec!["--db", "events.db"], vec!["--db"]] {
        let error = parse(&tail).unwrap_err();
        assert!(error.contains("--db is not accepted"), "{error}");
        assert!(error.contains("--user-config"), "{error}");
    }
}

#[test]
fn task_makes_the_project_canonical_and_keeps_the_filters() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a").join("..").join("b");
    std::fs::create_dir_all(dir.path().join("a")).unwrap();
    std::fs::create_dir_all(dir.path().join("b")).unwrap();
    let task = task(&ReindexArgs {
        project: Some(nested),
        since_ns: Some(5),
        include_evicted: true,
        wait: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        task.canonical_project.map(PathBuf::from),
        Some(dir.path().join("b").canonicalize().unwrap())
    );
    assert_eq!(task.since_ns, Some(5));
    assert!(task.include_evicted && task.wait);

    let file = dir.path().join("file");
    std::fs::write(&file, b"").unwrap();
    for project in [file, dir.path().join("missing")] {
        assert!(
            super::task(&ReindexArgs {
                project: Some(project),
                ..Default::default()
            })
            .is_err()
        );
    }
    assert_eq!(
        super::task(&ReindexArgs::default())
            .unwrap()
            .canonical_project,
        None
    );
}
