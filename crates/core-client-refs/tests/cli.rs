//! The command line: the baseline is read per file and a file that cannot be
//! read as Rust is an error, never a clean file.

use std::path::Path;
use std::process::{Command, Output};

fn run(root: &Path, baseline: &Path) -> Output {
    Command::new(std::env::var("CARGO_BIN_EXE_core-client-refs").unwrap())
        .arg("--root")
        .arg(root)
        .arg("--baseline")
        .arg(baseline)
        .output()
        .unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn tree() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    std::fs::create_dir_all(root.join("features")).unwrap();
    std::fs::write(
        root.join("mod.rs"),
        "use tokio::runtime::Builder;\nuse workbench_protocol::ids::*;\n",
    )
    .unwrap();
    (dir, root)
}

#[test]
fn a_new_reference_fails_and_a_baselined_one_passes() {
    let (dir, root) = tree();
    let baseline = dir.path().join("baseline.txt");
    std::fs::write(&baseline, "# none\n").unwrap();
    assert_eq!(run(&root, &baseline).status.code(), Some(0));

    let file = root.join("features/x.rs");
    std::fs::write(&file, "use storage::Database;\n").unwrap();
    let output = run(&root, &baseline);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("use of storage"),
        "{}",
        stderr(&output)
    );

    std::fs::write(&baseline, format!("{} 1\n", file.display())).unwrap();
    assert_eq!(run(&root, &baseline).status.code(), Some(0));

    std::fs::write(&file, "use storage::A;\nuse runtime::B;\n").unwrap();
    assert_eq!(run(&root, &baseline).status.code(), Some(1));
}

#[test]
fn a_file_that_is_not_rust_is_an_error() {
    let (dir, root) = tree();
    let baseline = dir.path().join("baseline.txt");
    std::fs::write(&baseline, "").unwrap();
    std::fs::write(root.join("features/broken.rs"), "fn f( {\n").unwrap();
    let output = run(&root, &baseline);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("not valid Rust"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn a_missing_baseline_or_root_is_an_error() {
    let (dir, root) = tree();
    assert_eq!(
        run(&root, &dir.path().join("none.txt")).status.code(),
        Some(2)
    );
    let baseline = dir.path().join("baseline.txt");
    std::fs::write(&baseline, "").unwrap();
    assert_eq!(
        run(&dir.path().join("none"), &baseline).status.code(),
        Some(2)
    );
}

#[test]
fn a_lowered_count_is_only_a_warning() {
    let (dir, root) = tree();
    let baseline = dir.path().join("baseline.txt");
    std::fs::write(&baseline, format!("{} 3\n", root.join("mod.rs").display())).unwrap();
    let output = run(&root, &baseline);
    assert_eq!(output.status.code(), Some(0));
    assert!(
        stderr(&output).contains("lower its baseline"),
        "{}",
        stderr(&output)
    );
}
