//! Minimal libtest-compatible selection for the physical-bootstrap custom harnesses.
//! Listing never starts an instance; every selected case still uses its own real exec.
use std::ffi::OsString;

pub fn select<'a>(cases: &[&'a str], args: &[OsString]) -> Option<Vec<&'a str>> {
    let mut list = false;
    let mut exact = false;
    let mut ignored = false;
    let mut filters = Vec::new();
    let mut skips = Vec::new();
    let mut args = args
        .iter()
        .map(|arg| arg.to_str().expect("UTF-8 harness argument"));
    while let Some(arg) = args.next() {
        match arg {
            "--list" => list = true,
            "--exact" => exact = true,
            // These harnesses have no ignored cases. nextest lists them separately.
            "--ignored" => ignored = true,
            "--nocapture" | "--include-ignored" | "--show-output" | "--quiet" | "-q" => {}
            "--format" => {
                assert!(matches!(args.next(), Some("terse" | "pretty")));
            }
            "--format=terse" | "--format=pretty" => {}
            "--skip" => skips.push(args.next().expect("--skip requires a filter")),
            "--" => {
                filters.extend(args);
                break;
            }
            option if option.starts_with('-') => panic!("unsupported harness option {option}"),
            filter => filters.push(filter),
        }
    }
    let matches = |case: &str, filter: &str| {
        if exact {
            case == filter
        } else {
            case.contains(filter)
        }
    };
    let selected = cases
        .iter()
        .copied()
        .filter(|case| {
            !ignored
                && (filters.is_empty() || filters.iter().any(|filter| matches(case, filter)))
                && !skips.iter().any(|filter| matches(case, filter))
        })
        .collect::<Vec<_>>();
    if list {
        for case in selected {
            println!("{case}: test");
        }
        None
    } else {
        Some(selected)
    }
}
