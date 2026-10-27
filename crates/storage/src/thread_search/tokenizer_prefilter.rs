//! The exact evaluator is disabled without a trusted tokenizer capability.
//!
//! A fresh Connection is NOT proof of bundled-trigram provenance: SQLite
//! auto-extensions run during open and can shadow it before our factory sees
//! it. Name/version/probes or a first-observed methods identity cannot prove
//! either that provenance or equivalence to the persisted FTS index.
//! No capability is currently issued. Production always returns SQL NULL,
//! causing COALESCE to evaluate the original FTS predicate. In particular no
//! unknown tokenizer is instantiated, moved (Send), or invoked by the UDF.

use rusqlite::{Connection, functions::FunctionFlags};

pub(super) fn install(conn: &Connection) -> bool {
    // Replace even an existing same-name function; discovering it by name is
    // not evidence of origin. No tokenizer API is consulted on this path.
    conn.create_scalar_function(
        c"evorch_fts5_exact",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        |_| Ok(None::<bool>),
    )
    .is_ok()
}

// Prior differential implementation is test-only; no production capability
// exists. Keep it available to the existing unknown-token/reference tests.
#[cfg(test)]
use std::cell::{Cell, RefCell};
#[cfg(test)]
use std::ffi::{c_int, c_void};
#[cfg(test)]
use std::ops::Range;
#[cfg(test)]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(test)]
use std::ptr;

#[cfg(test)]
use rusqlite::ffi;
#[cfg(test)]
use rusqlite::types::{ToSql, ToSqlOutput};

// FTS5 public API flags (sqlite3.h). Not exported by libsqlite3-sys.
#[cfg(test)]
const QUERY: c_int = 0x0001;
#[cfg(test)]
const DOCUMENT: c_int = 0x0004;
// An optimization must not allocate unbounded query state. Exceeding either
// bound yields NULL, falling back to the original FTS predicate.
#[cfg(test)]
const MAX_PHRASES: usize = 256;
#[cfg(test)]
const MAX_TOKENS: usize = 4096;

/// Install once per connection, lazily. Failure leaves the original SQL path.
#[cfg(test)]
pub(super) fn install_untrusted_for_tests(conn: &Connection) -> bool {
    // Unknown schemas/settings (including detail, columns and tokenizer
    // options) have no equivalence proof. Check even after installing the UDF:
    // a schema replacement on the same connection must disable the fast path.
    const SCHEMA: &str = "CREATE VIRTUAL TABLE conversation_fts USING fts5(
    text, content = conversation_blocks, content_rowid = id,
    tokenize = 'trigram', columnsize = 0
)";
    if conn
        .query_row(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = 'conversation_fts'",
            [],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .as_deref()
        != Some(SCHEMA)
    {
        return false;
    }
    if conn.prepare("SELECT evorch_fts5_exact('', '[]')").is_ok() {
        return true;
    }
    let Some(tokenizer) = Tokenizer::new(conn) else {
        return false;
    };
    register(conn, tokenizer)
}

#[cfg(test)]
fn register(conn: &Connection, tokenizer: Tokenizer) -> bool {
    let tokenizer = RefCell::new(tokenizer);
    conn.create_scalar_function(
        c"evorch_fts5_exact",
        2,
        FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            // Auxdata caches compiled QUERY tokens for the constant second
            // parameter, never a document or a previously returned result.
            let result = catch_unwind(AssertUnwindSafe(|| {
                let mut tokenizer = tokenizer.try_borrow_mut().ok()?;
                let compiled = match ctx.get_aux::<Option<Compiled>>(1).ok()? {
                    Some(compiled) => compiled,
                    None => {
                        let json = ctx.get_raw(1).as_str().ok()?;
                        let compiled = Compiled::new(&mut tokenizer, json);
                        ctx.set_aux(1, compiled).ok()?
                    }
                };
                let compiled = compiled.as_ref().as_ref()?;
                let text = ctx.get_raw(0).as_str().ok()?;
                compiled.matches(&mut tokenizer, text)
            }));
            // Unknown input, lengths, flags, tokenizer errors and Rust panics
            // cannot decide a leaf (nor escape the SQLite callback). SQL
            // COALESCE must evaluate the original MATCH in exactly this case.
            Ok(result.ok().flatten())
        },
    )
    .is_ok()
}

#[cfg(test)]
struct ApiPointer(Cell<*mut ffi::fts5_api>);

#[cfg(test)]
impl ToSql for ApiPointer {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        // FTS5 writes the API pointer synchronously during query_row. Cell
        // permits this write; it and the statement outlive that call. SQLite
        // does not own this pointer and must not run a destructor.
        Ok(ToSqlOutput::Pointer((
            self.0.as_ptr().cast(),
            c"fts5_api_ptr",
            None,
        )))
    }
}

#[cfg(test)]
struct Tokenizer {
    methods: ffi::fts5_tokenizer,
    instance: *mut ffi::Fts5Tokenizer,
}

// SAFETY (test reference only): fresh test connections control registration.
// This assertion is NOT valid for production, which never builds this owner.
// Its xCreate ignores the connection context and owns a sqlite3_malloc'd pair
// of settings; xTokenize uses that state and local buffers only. No connection,
// statement or callback pointer survives a call. Moving this owner is safe;
// it is deliberately not Sync. RefCell serializes use in the SQL closure.
#[cfg(test)]
unsafe impl Send for Tokenizer {}

#[cfg(test)]
impl Tokenizer {
    fn new(conn: &Connection) -> Option<Self> {
        let api = ApiPointer(Cell::new(ptr::null_mut()));
        // query_row owns/finalizes its statement before returning, including
        // on error; the bound pointer is not retained past this call.
        conn.query_row("SELECT fts5(?1)", [&api], |_| Ok(())).ok()?;
        let api = api.0.get();
        if api.is_null() {
            return None;
        }
        let mut methods = ffi::fts5_tokenizer {
            xCreate: None,
            xDelete: None,
            xTokenize: None,
        };
        let mut context = ptr::null_mut();
        // SAFETY: FTS5 supplied a valid API on this live connection. Output
        // slots and the static NUL-terminated tokenizer name are valid.
        let rc = unsafe {
            ((*api).xFindTokenizer?)(api, c"trigram".as_ptr(), &mut context, &mut methods)
        };
        if rc != ffi::SQLITE_OK {
            return None;
        }
        let create = methods.xCreate?;
        methods.xDelete?;
        methods.xTokenize?;
        let mut instance = ptr::null_mut();
        // SAFETY: These are the methods/context returned together by SQLite.
        // No args is exactly v27.sql's tokenize='trigram': defaults
        // case_sensitive=0, remove_diacritics=0. Keep these in sync.
        let rc = unsafe { create(context, ptr::null_mut(), 0, &mut instance) };
        if rc != ffi::SQLITE_OK || instance.is_null() {
            return None;
        }
        Some(Self { methods, instance })
    }

    fn tokenize(
        &mut self,
        text: &str,
        flags: c_int,
        context: *mut c_void,
        callback: unsafe extern "C" fn(
            *mut c_void,
            c_int,
            *const std::ffi::c_char,
            c_int,
            c_int,
            c_int,
        ) -> c_int,
    ) -> Option<()> {
        let len = c_int::try_from(text.len()).ok()?;
        // SAFETY: instance is owned until Drop. UTF-8 text and callback state
        // remain alive for this synchronous call. Use byte length, not a
        // C string: embedded NULs must reach SQLite unchanged.
        let rc = unsafe {
            (self.methods.xTokenize?)(
                self.instance,
                context,
                flags,
                text.as_ptr().cast(),
                len,
                Some(callback),
            )
        };
        (rc == ffi::SQLITE_OK).then_some(())
    }
}

#[cfg(test)]
impl Drop for Tokenizer {
    fn drop(&mut self) {
        // SAFETY: successful xCreate has exactly one matching xDelete, from
        // the same SQLite library. No callback or borrowed text is retained.
        unsafe { (self.methods.xDelete.unwrap())(self.instance) };
    }
}

#[cfg(test)]
struct Pattern {
    tokens: Vec<Vec<u8>>,
    fallback: Vec<usize>,
}

#[cfg(test)]
impl Pattern {
    fn new(tokenizer: &mut Tokenizer, text: &str) -> Option<Self> {
        // Generated MATCH literals never contain NUL (term_route scans them).
        // Raw MATCH's parser rejects NUL even though xTokenize accepts it.
        if text.contains('\0') {
            return None;
        }
        let mut tokens: Vec<Vec<u8>> = Vec::new();
        unsafe extern "C" fn collect(
            ctx: *mut c_void,
            flags: c_int,
            token: *const std::ffi::c_char,
            len: c_int,
            _: c_int,
            _: c_int,
        ) -> c_int {
            // Never unwind through SQLite. Nonzero flags include colocated
            // synonyms: their position semantics are outside this proof.
            catch_unwind(AssertUnwindSafe(|| {
                if flags != 0 || token.is_null() || !(1..=32).contains(&len) {
                    return ffi::SQLITE_ABORT;
                }
                // SAFETY: ctx is the exclusive Vec passed below; SQLite
                // guarantees token bytes are readable during the callback.
                let tokens = unsafe { &mut *ctx.cast::<Vec<Vec<u8>>>() };
                if tokens.len() >= MAX_TOKENS {
                    return ffi::SQLITE_ABORT;
                }
                let bytes = unsafe { std::slice::from_raw_parts(token.cast(), len as usize) };
                tokens.push(bytes.to_vec());
                ffi::SQLITE_OK
            }))
            .unwrap_or(ffi::SQLITE_ABORT)
        }
        tokenizer.tokenize(text, QUERY, ptr::from_mut(&mut tokens).cast(), collect)?;
        if tokens.is_empty() {
            return None;
        }
        // KMP supports repeated/overlapping triples without buffering the
        // document or quadratic rescans of long repeated-token phrases.
        let mut fallback = vec![0; tokens.len()];
        for i in 1..tokens.len() {
            let mut j = fallback[i - 1];
            while j > 0 && tokens[i] != tokens[j] {
                j = fallback[j - 1];
            }
            if tokens[i] == tokens[j] {
                j += 1;
            }
            fallback[i] = j;
        }
        Some(Self { tokens, fallback })
    }
}

#[cfg(test)]
struct Compiled {
    // OR alternatives / AND terms / OR variant ranges into patterns.
    alternatives: Vec<Vec<Range<usize>>>,
    patterns: Vec<Pattern>,
}

#[cfg(test)]
impl Compiled {
    fn new(tokenizer: &mut Tokenizer, json: &str) -> Option<Self> {
        let alternatives: Vec<Vec<Vec<String>>> = serde_json::from_str(json).ok()?;
        if alternatives.is_empty() {
            return None;
        }
        let mut compiled = Self {
            alternatives: Vec::new(),
            patterns: Vec::new(),
        };
        for terms in alternatives {
            if terms.is_empty() {
                return None;
            }
            let mut ranges = Vec::new();
            for variants in terms {
                if variants.is_empty() {
                    return None;
                }
                let start = compiled.patterns.len();
                for variant in variants {
                    if compiled.patterns.len() >= MAX_PHRASES {
                        return None;
                    }
                    let pattern = Pattern::new(tokenizer, &variant)?;
                    // k/s spellings often yield identical SQLite tokens.
                    // Avoid repeating the same automaton for every document
                    // token while retaining the exact OR of variants.
                    if !compiled.patterns[start..]
                        .iter()
                        .any(|previous| previous.tokens == pattern.tokens)
                    {
                        compiled.patterns.push(pattern);
                    }
                }
                ranges.push(start..compiled.patterns.len());
            }
            compiled.alternatives.push(ranges);
        }
        Some(compiled)
    }

    fn matches(&self, tokenizer: &mut Tokenizer, text: &str) -> Option<bool> {
        let mut state = Document {
            compiled: self,
            progress: vec![0; self.patterns.len()],
        };
        tokenizer.tokenize(text, DOCUMENT, ptr::from_mut(&mut state).cast(), document)?;
        Some(self.alternatives.iter().any(|terms| {
            terms.iter().all(|variants| {
                variants
                    .clone()
                    .any(|i| state.progress[i] == self.patterns[i].tokens.len())
            })
        }))
    }
}

#[cfg(test)]
struct Document<'a> {
    compiled: &'a Compiled,
    progress: Vec<usize>,
}

#[cfg(test)]
unsafe extern "C" fn document(
    ctx: *mut c_void,
    flags: c_int,
    token: *const std::ffi::c_char,
    len: c_int,
    _: c_int,
    _: c_int,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if flags != 0 || token.is_null() || !(1..=32).contains(&len) {
            return ffi::SQLITE_ABORT;
        }
        // SAFETY: ctx exclusively borrows the stack state for this call.
        // SQLite guarantees token bytes live for the callback duration.
        let state = unsafe { &mut *ctx.cast::<Document<'_>>() };
        let bytes = unsafe { std::slice::from_raw_parts(token.cast::<u8>(), len as usize) };
        for (pattern, progress) in state.compiled.patterns.iter().zip(&mut state.progress) {
            if *progress == pattern.tokens.len() {
                continue;
            }
            while *progress > 0 && pattern.tokens[*progress] != bytes {
                *progress = pattern.fallback[*progress - 1];
            }
            if pattern.tokens[*progress] == bytes {
                *progress += 1;
            }
        }
        ffi::SQLITE_OK
    }))
    .unwrap_or(ffi::SQLITE_ABORT)
}

#[cfg(test)]
mod tests {
    use super::install_untrusted_for_tests as install;
    use super::*;

    #[test]
    fn autoextension_shadow_never_creates_a_production_matcher() {
        const CHILD: &str = "EVORCH_TEST_UNKNOWN_TRIGRAM_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Auto-extension registration is process-global. Isolate this
            // adversarial registration from every parallel storage test.
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "thread_search::tokenizer_prefilter::tests::autoextension_shadow_never_creates_a_production_matcher"])
                .env(CHILD, "1").output().unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            return;
        }
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CREATES: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn deny_create(
            _: *mut c_void,
            _: *mut *const std::ffi::c_char,
            _: c_int,
            _: *mut *mut ffi::Fts5Tokenizer,
        ) -> c_int {
            CREATES.fetch_add(1, Ordering::Relaxed);
            ffi::SQLITE_ERROR
        }
        unsafe extern "C" fn shadow(
            db: *mut ffi::sqlite3,
            _: *mut *mut std::ffi::c_char,
            _: *const ffi::sqlite3_api_routines,
        ) -> c_int {
            // SAFETY: SQLite supplied this live connection; from_handle is
            // non-owning. The callback only registers methods, not instances.
            let conn = unsafe { Connection::from_handle(db) }.unwrap();
            let api = ApiPointer(Cell::new(ptr::null_mut()));
            conn.query_row("SELECT fts5(?1)", [&api], |_| Ok(()))
                .unwrap();
            let api = api.0.get();
            assert!(!api.is_null());
            let mut methods = ffi::fts5_tokenizer {
                xCreate: None,
                xDelete: None,
                xTokenize: None,
            };
            let mut ctx = ptr::null_mut();
            // SAFETY: API is supplied by FTS5 on this connection. SQLite
            // copies this methods table; all replacement functions are static.
            unsafe {
                let rc = ((*api).xFindTokenizer.unwrap())(
                    api,
                    c"trigram".as_ptr(),
                    &mut ctx,
                    &mut methods,
                );
                if rc != ffi::SQLITE_OK {
                    return rc;
                }
                methods.xCreate = Some(deny_create);
                ((*api).xCreateTokenizer.unwrap())(
                    api,
                    c"trigram".as_ptr(),
                    ctx,
                    &mut methods,
                    None,
                )
            }
        }
        // SAFETY: isolated child process; static callback only registers FTS
        // methods and never retains the supplied connection.
        unsafe { rusqlite::auto_extension::register_auto_extension(shadow) }.unwrap();
        let conn = Connection::open_in_memory().unwrap();
        rusqlite::auto_extension::cancel_auto_extension(shadow);
        assert!(super::install(&conn));
        assert_eq!(
            conn.query_row(
                "SELECT evorch_fts5_exact('abc', '[[[\"abc\"]]]')",
                [],
                |row| row.get::<_, Option<bool>>(0)
            )
            .unwrap(),
            None
        );
        assert_eq!(CREATES.load(Ordering::Relaxed), 0);
        // The shadow really is installed: a normal FTS table creation reaches
        // its xCreate. Our NULL-only evaluator did not reach it first.
        assert!(
            conn.execute_batch(
                "CREATE VIRTUAL TABLE shadow_probe USING fts5(text, tokenize='trigram')"
            )
            .is_err()
        );
        assert_eq!(CREATES.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn uncertain_or_oversized_query_state_returns_sql_null() {
        let conn = crate::Database::open_in_memory().unwrap().conn;
        assert!(install(&conn));
        for json in [
            "not json".to_owned(),
            "[]".into(),
            "[[]]".into(),
            "[[[]]]".into(),
            "[[[\"ab\"]]]".into(),          // empty QUERY token stream
            "[[[\"ab\\u0000cd\"]]]".into(), // not a generated MATCH literal
            serde_json::to_string(&vec![vec![vec!["a".repeat(MAX_TOKENS + 3)]]]).unwrap(),
            serde_json::to_string(&vec![vec![vec!["abc"]]; MAX_PHRASES + 1]).unwrap(),
        ] {
            assert_eq!(
                conn.query_row(
                    "SELECT evorch_fts5_exact('unrelated 日本語', ?1)",
                    [&json],
                    |row| row.get::<_, Option<bool>>(0),
                )
                .unwrap(),
                None,
                "{json:.100}"
            );
        }
        assert_eq!(
            conn.query_row("SELECT evorch_fts5_exact(NULL, NULL)", [], |row| {
                row.get::<_, Option<bool>>(0)
            })
            .unwrap(),
            None,
        );
    }

    #[test]
    fn unknown_document_flags_and_tokenizer_errors_return_sql_null() {
        let conn = Connection::open_in_memory().unwrap();
        let mut tokenizer = Tokenizer::new(&conn).unwrap();
        let json = "[[[\"abc\"]]]";
        let compiled = Compiled::new(&mut tokenizer, json).unwrap();
        let mut state = Document {
            compiled: &compiled,
            progress: vec![0],
        };
        // SAFETY: Same state/token lifetimes as the real synchronous callback.
        let rc = unsafe {
            document(
                ptr::from_mut(&mut state).cast(),
                1, // FTS5_TOKEN_COLOCATED: not a new consecutive position.
                c"abc".as_ptr(),
                3,
                0,
                3,
            )
        };
        assert_eq!(rc, ffi::SQLITE_ABORT);
        assert_eq!(state.progress, [0]);

        unsafe extern "C" fn fail(
            _: *mut ffi::Fts5Tokenizer,
            _: *mut c_void,
            _: c_int,
            _: *const std::ffi::c_char,
            _: c_int,
            _: Option<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_int,
                    *const std::ffi::c_char,
                    c_int,
                    c_int,
                    c_int,
                ) -> c_int,
            >,
        ) -> c_int {
            ffi::SQLITE_ABORT
        }
        tokenizer.methods.xTokenize = Some(fail);
        assert_eq!(compiled.matches(&mut tokenizer, "abc"), None);
        assert!(register(&conn, tokenizer));
        assert_eq!(
            conn.query_row("SELECT evorch_fts5_exact('abc', ?1)", [json], |row| row
                .get::<_, Option<
                bool,
            >>(
                0
            ),)
                .unwrap(),
            None
        );

        unsafe extern "C" fn unknown_after_match(
            _: *mut ffi::Fts5Tokenizer,
            ctx: *mut c_void,
            flags: c_int,
            _: *const std::ffi::c_char,
            _: c_int,
            callback: Option<
                unsafe extern "C" fn(
                    *mut c_void,
                    c_int,
                    *const std::ffi::c_char,
                    c_int,
                    c_int,
                    c_int,
                ) -> c_int,
            >,
        ) -> c_int {
            let callback = callback.unwrap();
            // SAFETY: forward only to SQLite's synchronous callback using
            // its borrowed ctx and static readable token, just as xTokenize.
            unsafe {
                let rc = callback(ctx, 0, c"abc".as_ptr(), 3, 0, 3);
                if rc != ffi::SQLITE_OK || flags == QUERY {
                    return rc;
                }
                callback(ctx, 1, c"abc".as_ptr(), 3, 1, 4)
            }
        }
        let mut tokenizer = Tokenizer::new(&conn).unwrap();
        tokenizer.methods.xTokenize = Some(unknown_after_match);
        assert!(register(&conn, tokenizer));
        assert_eq!(
            conn.query_row("SELECT evorch_fts5_exact('abc', ?1)", [json], |row| row
                .get::<_, Option<
                bool,
            >>(
                0
            ),)
                .unwrap(),
            None // even an already matched phrase cannot hide unknown flags
        );
    }
}
