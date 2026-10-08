//! Every `DiagnosticEvent` producer names its code through `diagnostic_codes`, and
//! `ALL` lists every constant, so the self-improvement classifier's pinned table
//! (which must cover `ALL`) makes an explicit decision for each emitted code.

use std::fs;
use std::path::{Path, PathBuf};

use event_bus::event::diagnostic_codes;
use regex::Regex;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|name| name != "tests") {
                rust_files(&path, out);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs")
            && !path.to_string_lossy().ends_with("tests.rs")
        {
            out.push(path);
        }
    }
}

/// Blanks comments and string/char literal contents, keeping byte offsets, so brace
/// matching only sees code.
fn mask(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    let blank = |out: &mut Vec<u8>, from: usize, to: usize| {
        for byte in &mut out[from..to] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    };
    while i < bytes.len() {
        let rest = &bytes[i..];
        if rest.starts_with(b"//") {
            let end = rest
                .iter()
                .position(|&b| b == b'\n')
                .map_or(bytes.len(), |n| i + n);
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(b"/*") {
            let end = source[i + 2..]
                .find("*/")
                .map_or(bytes.len(), |n| i + 2 + n + 2);
            blank(&mut out, i, end);
            i = end;
        } else if rest.starts_with(b"r#") || rest.starts_with(b"r\"") {
            let hashes = rest[1..].iter().take_while(|&&b| b == b'#').count();
            let close = format!("\"{}", "#".repeat(hashes));
            let body = i + 2 + hashes;
            let end = source[body..]
                .find(&close)
                .map_or(bytes.len(), |n| body + n + close.len());
            blank(&mut out, i, end);
            i = end;
        } else if rest[0] == b'"' {
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] != b'"' {
                j += if bytes[j] == b'\\' { 2 } else { 1 };
            }
            blank(&mut out, i, (j + 1).min(bytes.len()));
            i = j + 1;
        } else if rest[0] == b'\'' {
            // A char literal closes within a few bytes; otherwise this is a lifetime.
            let end = if rest.get(1) == Some(&b'\\') {
                rest.iter().skip(2).position(|&b| b == b'\'').map(|n| n + 3)
            } else {
                let width = source[i + 1..].chars().next().map_or(1, char::len_utf8);
                (rest.get(1 + width) == Some(&b'\'')).then_some(2 + width)
            };
            match end {
                Some(len) => {
                    blank(&mut out, i, i + len);
                    i += len;
                }
                None => i += 1,
            }
        } else {
            i += 1;
        }
    }
    out
}

/// Index just past the `}` closing the `{` at `open` in masked source.
fn block_end(masked: &[u8], open: usize) -> usize {
    let mut depth = 0usize;
    for (offset, byte) in masked.iter().enumerate().skip(open) {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return offset + 1;
                }
            }
            _ => {}
        }
    }
    panic!("unbalanced braces after offset {open}");
}

/// Drops `#[cfg(test)]` items (inline test modules and test-only helpers).
fn without_test_items(source: &str) -> String {
    let mut kept = String::new();
    let mut rest = source.to_string();
    loop {
        let masked = mask(&rest);
        let Some(start) = find(&masked, b"#[cfg(test)]") else {
            break;
        };
        kept.push_str(&rest[..start]);
        let semi = find(&masked[start..], b";").map(|n| start + n);
        let open = find(&masked[start..], b"{").map(|n| start + n);
        let end = match (semi, open) {
            (Some(semi), Some(open)) if semi < open => semi + 1,
            (_, Some(open)) => block_end(&masked, open),
            (Some(semi), None) => semi + 1,
            (None, None) => rest.len(),
        };
        rest = rest[end..].to_string();
    }
    kept.push_str(&rest);
    kept
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn production_sources() -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    for krate in fs::read_dir(repo_root().join("crates")).unwrap().flatten() {
        let src = krate.path().join("src");
        if src.is_dir() {
            rust_files(&src, &mut files);
        }
    }
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let source = without_test_items(&fs::read_to_string(&path).unwrap());
            (path, source)
        })
        .collect()
}

#[test]
fn every_shared_code_constant_is_listed_in_all() {
    let source = fs::read_to_string(repo_root().join("crates/event-bus/src/event.rs")).unwrap();
    let start = source.find("pub mod diagnostic_codes {").unwrap();
    let open = start + source[start..].find('{').unwrap();
    let module = &source[start..block_end(&mask(&source), open)];
    let constant = Regex::new(r#"pub const \w+: &str = "([^"]+)";"#).unwrap();
    let values: Vec<_> = constant
        .captures_iter(module)
        .map(|captures| captures[1].to_string())
        .collect();
    assert_eq!(values.len(), diagnostic_codes::ALL.len());
    for value in values {
        assert!(
            diagnostic_codes::ALL.contains(&value.as_str()),
            "{value} is missing from diagnostic_codes::ALL"
        );
    }
}

#[test]
fn diagnostic_producers_use_shared_code_constants() {
    let literal_code = Regex::new(r#"\bcode:\s*""#).unwrap();
    // The GUI browser helper takes the code as its second argument.
    let literal_browser_code = Regex::new(r#"\bemit\(\s*&?bus,\s*""#).unwrap();
    let mut producers = 0;
    let mut offenders = Vec::new();
    for (path, source) in production_sources() {
        let relative = path.strip_prefix(repo_root()).unwrap_or(&path).display();
        let masked = mask(&source);
        for (start, _) in source.match_indices("DiagnosticEvent {") {
            if masked[start] == b' ' {
                continue;
            }
            if source[..start].ends_with("struct ") {
                continue;
            }
            producers += 1;
            let open = start + "DiagnosticEvent ".len();
            if literal_code.is_match(&source[open..block_end(&masked, open)]) {
                offenders.push(format!("{relative}: literal DiagnosticEvent code"));
            }
        }
        if literal_browser_code.is_match(&source) {
            offenders.push(format!("{relative}: literal browser diagnostic code"));
        }
    }
    // Guards the scan itself: losing producers would make the check vacuous.
    assert!(producers >= 15, "only {producers} producers found");
    assert!(offenders.is_empty(), "{offenders:#?}");
}
