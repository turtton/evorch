//! `core-client-refs --root DIR --baseline FILE`: fails when a file of the
//! core-client desktop references internal crates more often than the baseline
//! allows. Run it through `scripts/check-core-client-refs.sh`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use core_client_refs::{INTERNAL_CRATES, references};

const DEFAULT_ROOT: &str = "crates/gui/src/core_client";
const DEFAULT_BASELINE: &str = "scripts/core-client-refs-baseline.txt";

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)?
        .map(|entry| entry.map(|e| e.path()))
        .collect::<Result<_, _>>()?;
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out)?;
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

fn read_baseline(path: &Path) -> Result<BTreeMap<String, usize>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| format!("cannot read {}: {error}", path.display()))?;
    let mut allowed = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (file, count) = line
            .rsplit_once(char::is_whitespace)
            .ok_or_else(|| format!("bad baseline line: {line}"))?;
        let count = count
            .parse()
            .map_err(|_| format!("bad count in baseline line: {line}"))?;
        allowed.insert(file.trim().to_owned(), count);
    }
    Ok(allowed)
}

fn run() -> Result<bool, String> {
    let mut root = PathBuf::from(DEFAULT_ROOT);
    let mut baseline = PathBuf::from(DEFAULT_BASELINE);
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value =
            |name: &str, next: Option<String>| next.ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--root" => root = PathBuf::from(value("--root", args.next())?),
            "--baseline" => baseline = PathBuf::from(value("--baseline", args.next())?),
            other => return Err(format!("unknown option: {other}")),
        }
    }
    let internal: BTreeSet<String> = INTERNAL_CRATES.iter().map(|s| (*s).to_owned()).collect();
    let allowed = read_baseline(&baseline)?;

    let mut files = Vec::new();
    rust_files(&root, &mut files)
        .map_err(|error| format!("cannot read {}: {error}", root.display()))?;
    let mut failures = 0;
    for file in &files {
        let name = file.to_string_lossy().into_owned();
        let source = std::fs::read_to_string(file)
            .map_err(|error| format!("cannot read {name}: {error}"))?;
        let found = references(&source, &internal)
            .map_err(|error| format!("{name} is not valid Rust: {error}"))?;
        let limit = allowed.get(&name).copied().unwrap_or(0);
        if found.len() > limit {
            eprintln!(
                "FAIL: {name} has {} reference(s) to internal crates; the baseline allows {limit}. Reach the core through the protocol:",
                found.len()
            );
            for reference in &found {
                eprintln!("  {name}: {}", reference.description);
            }
            failures += 1;
        } else if found.len() < limit {
            eprintln!(
                "warning: {name} is down to {} reference(s); lower its baseline from {limit}",
                found.len()
            );
        }
    }
    for name in allowed.keys() {
        if !Path::new(name).is_file() {
            eprintln!("warning: baseline names a file that does not exist: {name}");
        }
    }
    Ok(failures == 0)
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => {
            println!("core-client references are within the baseline");
            ExitCode::SUCCESS
        }
        Ok(false) => ExitCode::from(1),
        Err(message) => {
            eprintln!("{message}");
            ExitCode::from(2)
        }
    }
}
