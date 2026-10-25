use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
enum Entry {
    Dir,
    File(Vec<u8>),
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Entry> {
    fn visit(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, Entry>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(root).unwrap().to_path_buf();
            if path.is_dir() {
                out.insert(rel, Entry::Dir);
                visit(root, &path, out);
            } else {
                out.insert(rel, Entry::File(fs::read(path).unwrap()));
            }
        }
    }
    let mut out = BTreeMap::new();
    if root.exists() {
        visit(root, root, &mut out);
    }
    out
}

fn copy_dir(src: &Path, dest: &Path) {
    if !src.exists() {
        return;
    }
    for entry in fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        let target = dest.join(entry.file_name());
        if path.is_dir() {
            fs::create_dir_all(&target).unwrap();
            copy_dir(&path, &target);
        } else {
            fs::copy(path, target).unwrap();
        }
    }
}

#[test]
fn upstream_scenarios_001_through_024() {
    let scenarios = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/scenarios");
    let mut count = 0;
    for scenario in fs::read_dir(scenarios).unwrap() {
        let scenario = scenario.unwrap().path();
        if !scenario.is_dir() {
            continue;
        }
        count += 1;
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&scenario.join("input"), dir.path());
        let patch = fs::read_to_string(scenario.join("patch.txt")).unwrap();
        let before = snapshot(dir.path());
        let result = apply_patch::apply_patch(&patch, Some(dir.path()));
        let name = scenario.file_name().unwrap().to_str().unwrap();
        let should_fail = matches!(
            &name[..3],
            "005" | "006" | "007" | "008" | "009" | "012" | "013" | "015"
        );
        assert_eq!(result.is_err(), should_fail, "{name}: {result:?}");
        let expected = if name == "015_failure_after_partial_success_leaves_changes" {
            // Intentional difference: a later match failure is discovered during
            // read-only planning, so the earlier add must not reach disk.
            assert!(result.is_err());
            before
        } else {
            snapshot(&scenario.join("expected"))
        };
        assert_eq!(
            snapshot(dir.path()),
            expected,
            "{}: {result:?}",
            scenario.display()
        );
    }
    assert_eq!(count, 25); // upstream includes two different 020 scenarios
}
