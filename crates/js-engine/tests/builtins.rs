//! Behaviour of rquickjs / QuickJS-NG as measured on the real engine (ADR 0039,
//! "Open questions": built-ins were only checked against documentation).
//!
//! The expectations below are what the run_js description promises models, so a
//! failure here after an engine upgrade means the description must change.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Duration;

use async_trait::async_trait;
use js_engine::{HostCalls, RunOptions, RunOutcome, run};
use serde_json::{Value, json};

#[derive(Default)]
struct FakeHost {
    printed: RefCell<String>,
    calls: RefCell<Vec<(String, Value)>>,
}

#[async_trait(?Send)]
impl HostCalls for FakeHost {
    async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
        self.calls
            .borrow_mut()
            .push((name.to_string(), args.clone()));
        match name {
            "echo" => {
                Ok(json!({"ok": true, "text": args["text"], "truncated": false, "next": null}))
            }
            "boom" => Err("denied: use a normal tool call".to_string()),
            _ => Err(format!("unknown tool {name}")),
        }
    }

    fn output(&self, text: &str) {
        self.printed.borrow_mut().push_str(text);
    }
}

fn options() -> RunOptions {
    RunOptions {
        timeout: Duration::from_secs(60),
        memory_limit_bytes: 64 * 1024 * 1024,
        tools: vec!["echo".into(), "boom".into()],
    }
}

async fn eval(code: &str) -> RunOutcome {
    eval_with(code, options()).await.0
}

async fn eval_with(code: &str, options: RunOptions) -> (RunOutcome, Rc<FakeHost>) {
    let host = Rc::new(FakeHost::default());
    let outcome = run(code, options, host.clone()).await;
    (outcome, host)
}

async fn value_of(code: &str) -> Value {
    let outcome = eval(code).await;
    assert_eq!(outcome.error, None, "code: {code}");
    outcome.value
}

#[tokio::test]
async fn named_capture_groups_and_lookbehind() {
    assert_eq!(
        value_of(r#"return "2026-10-28".match(/(?<y>\d{4})-(?<m>\d{2})/).groups.m;"#).await,
        json!("10")
    );
    assert_eq!(
        value_of(r#"return "cost: $42 and €7".match(/(?<=\$)\d+/)[0];"#).await,
        json!("42")
    );
    assert_eq!(
        value_of(r#"return "ab12".replace(/(?<!a)b|(?<=b)1/, "_");"#).await,
        json!("ab_2")
    );
}

#[tokio::test]
async fn unicode_property_escapes_with_script() {
    assert_eq!(
        value_of(r#"return [..."abcひらがなカタカナ漢字"].filter(c => /\p{Script=Hiragana}/u.test(c)).join("");"#)
            .await,
        json!("ひらがな")
    );
    assert_eq!(
        value_of(r#"return /^\p{Lu}\p{Ll}+$/u.test("Éclair");"#).await,
        json!(true)
    );
}

#[tokio::test]
async fn modern_string_and_array_methods() {
    assert_eq!(
        value_of(r#"return "a-b-c".replaceAll("-", "+");"#).await,
        json!("a+b+c")
    );
    assert_eq!(
        value_of(r#"return [..."a1b22c333".matchAll(/\d+/g)].map(m => m[0]);"#).await,
        json!(["1", "22", "333"])
    );
    assert_eq!(
        value_of(r#"return [[1, 2, 3].at(-1), "xyz".at(-2)];"#).await,
        json!([3, "y"])
    );
    assert_eq!(
        value_of(r#"return "7".padStart(3, "0") + "7".padEnd(3, ".");"#).await,
        json!("0077..")
    );
    assert_eq!(
        value_of(r#"return [[1, 2], [3]].flatMap(x => x);"#).await,
        json!([1, 2, 3])
    );
    assert_eq!(
        value_of(r#"return [1, 2, 3, 4].findLast(x => x % 2 === 1);"#).await,
        json!(3)
    );
    assert_eq!(
        value_of(r#"const a = [3, 1, 2]; const b = a.toSorted(); return [a, b];"#).await,
        json!([[3, 1, 2], [1, 2, 3]])
    );
    assert_eq!(
        value_of(r#"return Object.groupBy([1, 2, 3], x => x % 2 ? "odd" : "even");"#).await,
        json!({"odd": [1, 3], "even": [2]})
    );
    assert_eq!(
        value_of(r#"return [[...new Set([1, 1, 2])], typeof structuredClone];"#).await,
        json!([[1, 2], "undefined"])
    );
}

#[tokio::test]
async fn json_round_trips_non_ascii_and_surrogates() {
    assert_eq!(
        value_of(r#"return JSON.parse(JSON.stringify({s: "日本語 😀 é"})).s;"#).await,
        json!("日本語 😀 é")
    );
    // A lone surrogate is escaped by well-formed JSON.stringify and must round-trip.
    assert_eq!(
        value_of(r#"return JSON.stringify("\ud83d");"#).await,
        json!("\"\\ud83d\"")
    );
    assert_eq!(value_of(r#"return "😀".length;"#).await, json!(2));
    assert_eq!(value_of(r#"return [..."😀"].length;"#).await, json!(1));
}

#[tokio::test]
async fn intl_is_absent_and_locale_compare_uses_code_points() {
    assert_eq!(value_of(r#"return typeof Intl;"#).await, json!("undefined"));
    assert_eq!(
        value_of(r#"return ["b", "a", "C"].sort((x, y) => x.localeCompare(y));"#).await,
        json!(["C", "a", "b"])
    );
    assert_eq!(
        value_of(r#"return (1234.5).toLocaleString();"#).await,
        json!("1234.5")
    );
}

#[tokio::test]
async fn web_and_node_apis_are_undefined() {
    for name in [
        "setTimeout",
        "setInterval",
        "fetch",
        "require",
        "process",
        "module",
        "TextEncoder",
        "URL",
        "std",
        "os",
        "XMLHttpRequest",
        "WebAssembly",
    ] {
        assert_eq!(
            value_of(&format!("return typeof {name};")).await,
            json!("undefined"),
            "{name} must not exist"
        );
    }
    // import must not be usable either.
    let outcome = eval(r#"await import("os"); return 1;"#).await;
    assert!(
        outcome.error.is_some(),
        "dynamic import must fail: {outcome:?}"
    );
}

#[tokio::test]
async fn top_level_await_and_promise_all() {
    assert_eq!(
        value_of(r#"const v = await Promise.resolve(41); return v + 1;"#).await,
        json!(42)
    );
    assert_eq!(
        value_of(r#"const r = await Promise.all([1, 2, 3].map(async x => x * 2)); return r;"#)
            .await,
        json!([2, 4, 6])
    );
}

#[tokio::test]
async fn return_values_are_json_and_undefined_is_null() {
    assert_eq!(value_of("").await, Value::Null);
    assert_eq!(value_of("return undefined;").await, Value::Null);
    assert_eq!(
        value_of("return {a: undefined, b: 1};").await,
        json!({"b": 1})
    );
    assert_eq!(value_of("return () => 1;").await, Value::Null);
    let outcome = eval("return 10n;").await;
    assert!(outcome.error.is_some(), "BigInt is not JSON serialisable");
}

#[tokio::test]
async fn console_and_print_are_captured() {
    let (outcome, host) = eval_with(
        r#"console.log("a", 1, {k: [1]}); print("b"); console.error("c"); console.log(new Error("e").message);"#,
        options(),
    )
    .await;
    assert_eq!(outcome.error, None);
    assert_eq!(host.printed.borrow().as_str(), "a 1 {\"k\":[1]}\nb\nc\ne\n");
}

#[tokio::test]
async fn tools_calls_run_concurrently_through_the_host() {
    let (outcome, host) = eval_with(
        r#"
        const rs = await Promise.all([tools.echo({text: "x"}), tools.echo({text: "y"})]);
        return rs.map(r => r.text);
        "#,
        options(),
    )
    .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.value, json!(["x", "y"]));
    assert_eq!(host.calls.borrow().len(), 2);
}

#[tokio::test]
async fn host_errors_reject_the_promise_with_the_message() {
    assert_eq!(
        value_of(r#"try { await tools.boom({}); } catch (e) { return e.message; }"#).await,
        json!("denied: use a normal tool call")
    );
    let outcome = eval(r#"await tools.boom({});"#).await;
    assert_eq!(
        outcome.error.unwrap().message,
        "Error: denied: use a normal tool call"
    );
}

#[tokio::test]
async fn read_text_follows_next_until_the_end() {
    struct Pages;
    #[async_trait(?Send)]
    impl HostCalls for Pages {
        async fn call_tool(&self, name: &str, args: Value) -> Result<Value, String> {
            assert_eq!(name, "read");
            let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1);
            Ok(if offset == 1 {
                json!({"ok": true, "text": "page1\n", "truncated": true,
                       "next": {"tool": "read", "path": args["path"], "offset": 2}})
            } else {
                json!({"ok": true, "text": "page2\n", "truncated": false, "next": null})
            })
        }
        fn output(&self, _: &str) {}
    }
    let outcome = run(
        r#"return await readText("a.txt");"#,
        RunOptions {
            tools: vec!["read".into()],
            ..options()
        },
        Rc::new(Pages),
    )
    .await;
    assert_eq!(outcome.error, None);
    assert_eq!(outcome.value, json!("page1\npage2\n"));
}

#[tokio::test]
async fn read_text_fails_beyond_four_mebibytes() {
    struct Endless;
    #[async_trait(?Send)]
    impl HostCalls for Endless {
        async fn call_tool(&self, _: &str, args: Value) -> Result<Value, String> {
            Ok(
                json!({"ok": true, "text": "x".repeat(1024 * 1024), "truncated": true,
                      "next": {"tool": "read", "path": args["path"], "offset": 2}}),
            )
        }
        fn output(&self, _: &str) {}
    }
    let outcome = run(
        r#"return await readText("big.txt");"#,
        RunOptions {
            tools: vec!["read".into()],
            ..options()
        },
        Rc::new(Endless),
    )
    .await;
    assert!(outcome.error.unwrap().message.contains("4 MiB"));
}

#[tokio::test]
async fn exception_stack_lines_match_the_user_code_lines() {
    let outcome = eval("const a = 1;\nconst b = 2;\nthrow new Error(\"bad\");\n").await;
    let error = outcome.error.expect("error");
    assert_eq!(error.message, "Error: bad");
    assert!(
        error.stack.contains("run_js:3"),
        "stack was {:?}",
        error.stack
    );
    assert!(
        !error.stack.contains("<prelude>"),
        "stack was {:?}",
        error.stack
    );

    let nested = eval("function f() {\n  null.x;\n}\nawait Promise.resolve();\nf();").await;
    let error = nested.error.expect("error");
    assert!(
        error.message.starts_with("TypeError"),
        "{:?}",
        error.message
    );
    assert!(
        error.stack.contains("run_js:2"),
        "stack was {:?}",
        error.stack
    );

    let syntax = eval("const a = 1;\nconst = ;").await;
    let error = syntax.error.expect("syntax error");
    assert!(error.message.contains("SyntaxError"), "{:?}", error.message);
    assert!(
        error.stack.contains("run_js:2"),
        "stack was {:?}",
        error.stack
    );
}

#[tokio::test]
async fn memory_limit_stops_runaway_allocation() {
    let (outcome, _) = eval_with(
        r#"const keep = []; for (;;) keep.push("x".repeat(1024 * 1024) + keep.length);"#,
        RunOptions {
            memory_limit_bytes: 16 * 1024 * 1024,
            ..options()
        },
    )
    .await;
    let error = outcome.error.expect("memory error");
    assert!(
        error.message.to_lowercase().contains("out of memory"),
        "{error:?}"
    );
    assert!(!outcome.timed_out);
}

#[tokio::test]
async fn deep_recursion_is_a_range_error_not_a_crash() {
    let outcome = eval("function f() { return f() + 1; }\nf();").await;
    let error = outcome.error.expect("stack error");
    assert!(error.message.contains("RangeError"), "{error:?}");
}

// The deadline is the behaviour under test here, so this uses a short real
// timeout; correctness does not depend on how long the loop actually ran (a hang
// would be caught by the runner's slow-timeout).
#[tokio::test]
async fn infinite_loops_are_interrupted_at_the_deadline() {
    let (outcome, _) = eval_with(
        "for (;;) {}",
        RunOptions {
            timeout: Duration::from_millis(100),
            ..options()
        },
    )
    .await;
    assert!(outcome.timed_out);
    assert!(outcome.error.unwrap().message.contains("timed out"));
}

#[tokio::test]
async fn deadline_also_covers_a_host_call_that_never_returns() {
    struct Hang;
    #[async_trait(?Send)]
    impl HostCalls for Hang {
        async fn call_tool(&self, _: &str, _: Value) -> Result<Value, String> {
            std::future::pending().await
        }
        fn output(&self, _: &str) {}
    }
    let outcome = run(
        r#"await tools.echo({});"#,
        RunOptions {
            timeout: Duration::from_millis(100),
            ..options()
        },
        Rc::new(Hang),
    )
    .await;
    assert!(outcome.timed_out);
}

#[tokio::test]
async fn output_beyond_the_cap_is_an_error() {
    let outcome =
        eval(r#"const line = "x".repeat(1024 * 1024); for (let i = 0; i < 40; i++) print(line);"#)
            .await;
    assert!(outcome.error.unwrap().message.contains("output limit"));
}

/// Measured differences from the ADR's assumptions: these exist in QuickJS-NG
/// even though the ADR lists only ECMAScript built-ins.
#[tokio::test]
async fn engine_extras_that_the_adr_did_not_assume() {
    assert_eq!(
        value_of(
            r#"return [typeof queueMicrotask, typeof performance, typeof atob, typeof WeakRef,
                       typeof Set.prototype.union, typeof Iterator, typeof eval];"#
        )
        .await,
        json!([
            "function", "object", "function", "function", "function", "function", "function"
        ])
    );
    // localeCompare normalises, then compares code points.
    assert_eq!(
        value_of(r#"return "a\u0301".localeCompare("á");"#).await,
        json!(0)
    );
    assert_eq!(
        value_of(r#"return "ä".localeCompare("z");"#).await,
        json!(1)
    );
    // The `v` flag (set notation) is supported.
    assert_eq!(
        value_of(r#"try { new RegExp("[\\p{L}--[a-z]]", "v"); return "ok"; } catch (e) { return e.name; }"#).await,
        json!("ok")
    );
}
