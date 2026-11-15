use std::collections::BTreeSet;

use super::{INTERNAL_CRATES, references};

fn count(source: &str) -> usize {
    let internal: BTreeSet<String> = INTERNAL_CRATES.iter().map(|s| (*s).to_owned()).collect();
    references(source, &internal)
        .unwrap_or_else(|error| panic!("the fixture is not Rust: {error}\n{source}"))
        .len()
}

/// Sources that name an internal crate: each one is a reference.
#[test]
fn every_spelling_of_a_crate_name_is_a_reference() {
    let cases = [
        ("a path", "fn f() { let _ = storage::db::open(); }"),
        ("a use", "use storage::Database;"),
        ("an absolute use", "use ::storage::Database;"),
        ("a raw identifier", "pub use r#storage::Database;"),
        ("a bare use", "use storage;"),
        ("an alias", "use runtime as rt;"),
        ("a group with an alias", "use {serde, sandbox as sb};"),
        ("a nested group", "use {serde, storage::{db, Database}};"),
        (
            "a multi-line group with an alias",
            "use {\n    runtime as rt,\n    serde,\n};",
        ),
        ("a self import with an alias", "use storage::{self as s};"),
        ("a glob import", "use storage::*;"),
        ("an extern crate", "extern crate providers;"),
        (
            "an absolute path in an expression",
            "fn f() { let _ = ::runtime::Runtime::new(); }",
        ),
        (
            "an absolute return type",
            "pub fn f() -> ::storage::Database { panic!() }",
        ),
        (
            "an absolute type in a match arm",
            "fn f(x: u8) { match x { 0 => ::storage::open(), _ => () } }",
        ),
        (
            "an absolute type argument",
            "type T = Option<::storage::Database>;",
        ),
        ("a type", "struct S { db: storage::Database }"),
        (
            "a pattern",
            "fn f(x: A) { match x { runtime::State::Idle => {} _ => {} } }",
        ),
        ("a trait bound", "fn f<T: sandbox::Sandbox>(_: T) {}"),
        ("an attribute path", "#[providers::export] fn f() {}"),
        ("a macro path", "fn f() { storage::trace!(1); }"),
        (
            "an expression in a macro",
            "fn f() { println!(\"{}\", storage::VERSION); }",
        ),
        (
            "an absolute path in a macro",
            "fn f() { assert!(::storage::ok()); }",
        ),
        (
            "a return type in a macro",
            "macro_rules! m { () => { fn g() -> ::storage::Database { panic!() } } }",
        ),
        (
            "a use in a macro definition",
            "macro_rules! m { () => { use storage::Db; } }",
        ),
        (
            "a crate named in a derive list",
            "#[derive(Debug, storage::Marker)] struct S;",
        ),
        (
            "an alias named like the crate it imports",
            "use storage::Database as storage;",
        ),
        ("a module of the crate named like it", "use tools::tools;"),
        (
            "a second leaf after an alias of the same name",
            "use {storage::A as storage, storage::B};",
        ),
        (
            "an absolute use in a macro definition",
            "macro_rules! m { () => { use ::storage::Database as Db; } }",
        ),
        (
            "an absolute path after return in a macro definition",
            "macro_rules! m { () => { return ::storage::Database } }",
        ),
        (
            "an absolute path after return in a call",
            "fn f() { m!(return ::storage::Database); }",
        ),
        (
            "an absolute path after pub(crate) in a macro",
            "macro_rules! m { () => { pub(crate) ::storage::X } }",
        ),
        (
            "a path after a text macro in the same call",
            "fn f() { m!(stringify!(a), storage::Database); }",
        ),
    ];
    for (what, source) in cases {
        assert!(count(source) > 0, "{what} must count: {source}");
    }
}

#[test]
fn a_path_through_an_alias_counts_in_addition_to_the_import() {
    assert_eq!(
        count("use runtime as rt;\nfn f() { rt::AgentRuntime::new(); }"),
        2
    );
    assert_eq!(count("use storage::{self as s};\nfn f() { s::open(); }"), 2);
}

#[test]
fn one_use_counts_once_per_root_crate() {
    assert_eq!(count("use config::mcp::{A, B, C};"), 1);
    assert_eq!(count("use {config::A, runtime::B, config::C};"), 2);
}

/// Sources that do not name an internal crate.
#[test]
fn what_only_looks_like_a_crate_name_is_not_a_reference() {
    let cases = [
        ("tokio runtime", "use tokio::runtime::Builder;"),
        (
            "an absolute tokio runtime",
            "use ::tokio::runtime::Builder;",
        ),
        (
            "a tokio alias in a group",
            "use {tokio::runtime as rt, serde};\nfn f() { let _ = rt::Builder::new_multi_thread(); }",
        ),
        (
            "a local module in a group",
            "use {super::config, serde};\nfn f() { config::load(); }",
        ),
        (
            "a super import",
            "use super::config;\nfn f() { config::load(); }",
        ),
        ("a crate import", "use crate::storage::Db;"),
        (
            "an identifier part",
            "fn f() { let storage_path = 1; let runtime_id = 2; }",
        ),
        (
            "a local variable",
            "fn f(storage: u8) -> u8 { storage + 1 }",
        ),
        ("a single segment", "fn f() { runtime(); }"),
        ("a method turbofish", "fn f(x: X) { x.config::<u8>(); }"),
        (
            "a string",
            "const A: &str = \"use storage::Db; ::runtime::X\";",
        ),
        ("a raw string", "const A: &str = r#\"use storage::Db;\"#;"),
        (
            "a comment",
            "// use runtime as rt;\n/* use sandbox::X; */\nfn f() {}",
        ),
        ("a doc comment", "/// See [`storage::Db`].\nfn f() {}"),
        (
            "a lifetime and character literals",
            "fn f<'a>(x: &'a str) -> char { let _ = '\\''; 'x' }",
        ),
        (
            "a macro metavariable",
            "macro_rules! m { ($storage:ident) => { $storage::x() } }",
        ),
        (
            "$crate in a macro",
            "macro_rules! m { () => { $crate::storage::x() } }",
        ),
        (
            "a method turbofish in a macro",
            "fn f() { vec![x.config::<u8>()]; }",
        ),
        (
            "a path that continues",
            "fn f() { tokio::runtime::Handle::current(); }",
        ),
        (
            "stringify",
            "fn f() { let _ = stringify!(storage::Database); }",
        ),
        (
            "stringify inside another macro",
            "fn f() { println!(\"{}\", stringify!(::storage::Database)); }",
        ),
        (
            "concat",
            "const A: &str = concat!(\"x\", stringify!(runtime::X));",
        ),
        (
            "a generic parameter named like a crate",
            "fn f<storage: Tr>(_: storage::Assoc) {}",
        ),
        (
            "a const generic named like a crate",
            "struct S<const config: usize>([u8; config::N]);",
        ),
        (
            "a generic parameter of an impl",
            "impl<runtime: Tr> S<runtime> { fn f(_: runtime::X) {} }",
        ),
    ];
    for (what, source) in cases {
        assert_eq!(count(source), 0, "{what} must not count: {source}");
    }
}

#[test]
fn a_name_the_scope_defines_hides_the_crate_there_only() {
    // A module of that name, imported from: it is not the crate.
    assert_eq!(
        count("mod storage { pub struct Database; }\nuse storage::Database;"),
        0
    );
    assert_eq!(
        count("mod runtime { pub fn f() {} }\nfn g() { runtime::f(); }"),
        0
    );
    assert_eq!(count("struct sandbox;\nfn g() { sandbox::f(); }"), 0);
    assert_eq!(count("use std as storage;\nuse storage::fmt;"), 0);
    // An absolute path is always the crate.
    assert_eq!(
        count("use std as storage;\nfn f() -> ::storage::Database { panic!() }"),
        1
    );
    assert_eq!(
        count("mod storage {}\nfn f() { let _ = ::storage::open(); }"),
        1
    );
    // A child module's name does not hide the crate outside it.
    assert_eq!(
        count("mod nested { mod storage {} }\npub fn f() -> Option<storage::Database> { None }"),
        1
    );
    // And the parent's names are not visible inside the child.
    assert_eq!(
        count("mod storage {}\nmod nested { pub fn f() -> storage::Database { panic!() } }"),
        1
    );
    // A block's item hides the crate in that block.
    assert_eq!(
        count(
            "fn f() { mod storage { pub fn open() {} } storage::open(); }\nfn g() { storage::open(); }"
        ),
        1
    );
    // A use inside a function is read like any other.
    assert_eq!(count("fn f() { use runtime::Runtime; }"), 1);
}

#[test]
fn an_unreadable_file_is_an_error_not_a_clean_file() {
    let internal: BTreeSet<String> = INTERNAL_CRATES.iter().map(|s| (*s).to_owned()).collect();
    assert!(references("fn f( {", &internal).is_err());
}

#[test]
fn a_generic_parameter_hides_the_crate_in_its_item_only() {
    assert_eq!(
        count("fn f<storage: Tr>(_: storage::Assoc) {}\nfn g(_: storage::Database) {}"),
        1
    );
}

#[test]
fn an_import_does_not_hide_its_own_source() {
    assert_eq!(count("use tools::tools;"), 1);
    assert_eq!(count("use storage::Database as storage;"), 1);
    assert_eq!(count("use {storage::A as storage, storage::B};"), 1);
    // Another declaration's import of that name still shadows the crate.
    assert_eq!(count("use std as storage;\nuse storage::fmt;"), 0);
}

#[test]
fn a_keyword_starts_the_path_after_it() {
    assert_eq!(
        count("macro_rules! m { () => { use ::storage::Database as Db; } }"),
        1
    );
    assert_eq!(
        count("macro_rules! m { () => { return ::storage::Database } }"),
        1
    );
    // A path segment before `::` still continues the path.
    assert_eq!(
        count("macro_rules! m { () => { crate::x::storage::y() } }"),
        0
    );
    assert_eq!(
        count("macro_rules! m { () => { std::mem::swap(a, b); self::storage::y() } }"),
        0
    );
}
