//! Conservative necessary phrase matching through the bundled SQLite API.
//! No Unicode tables, normalization or FTS expression parser live here.

use std::cell::{Cell, RefCell};
use std::ffi::{c_int, c_void};
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use rusqlite::functions::FunctionFlags;
use rusqlite::types::{ToSql, ToSqlOutput};
use rusqlite::{Connection, ffi};

// FTS5 public API flags (sqlite3.h). Not exported by libsqlite3-sys.
const QUERY: c_int = 0x0001;
const DOCUMENT: c_int = 0x0004;
// An optimization must not allocate unbounded query state. Exceeding either
// bound simply passes through to the original FTS predicate.
const MAX_PHRASES: usize = 256;
const MAX_TOKENS: usize = 4096;

/// Install once per connection, lazily. Failure leaves the original SQL path.
pub(super) fn install(conn: &Connection) -> bool {
    if conn
        .prepare("SELECT evorch_fts5_necessary('', '[]')")
        .is_ok()
    {
        return true;
    }
    let Some(tokenizer) = Tokenizer::new(conn) else {
        return false;
    };
    let tokenizer = RefCell::new(tokenizer);
    conn.create_scalar_function(
        c"evorch_fts5_necessary",
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
            // cannot remove a candidate (nor escape the SQLite callback).
            Ok(result.ok().flatten().unwrap_or(true))
        },
    )
    .is_ok()
}

struct ApiPointer(Cell<*mut ffi::fts5_api>);

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

struct Tokenizer {
    methods: ffi::fts5_tokenizer,
    instance: *mut ffi::Fts5Tokenizer,
}

// SAFETY: Only the bundled built-in trigram tokenizer is instantiated here.
// Its xCreate ignores the connection context and owns a sqlite3_malloc'd pair
// of settings; xTokenize uses that state and local buffers only. No connection,
// statement or callback pointer survives a call. Moving this owner is safe;
// it is deliberately not Sync. RefCell serializes use in the SQL closure.
unsafe impl Send for Tokenizer {}

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
        // No args is exactly v25.sql's tokenize='trigram': defaults
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

impl Drop for Tokenizer {
    fn drop(&mut self) {
        // SAFETY: successful xCreate has exactly one matching xDelete, from
        // the same SQLite library. No callback or borrowed text is retained.
        unsafe { (self.methods.xDelete.unwrap())(self.instance) };
    }
}

struct Pattern {
    tokens: Vec<Vec<u8>>,
    fallback: Vec<usize>,
}

impl Pattern {
    fn new(tokenizer: &mut Tokenizer, text: &str) -> Option<Self> {
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

struct Compiled {
    // OR alternatives / AND terms / OR variant ranges into patterns.
    alternatives: Vec<Vec<Range<usize>>>,
    patterns: Vec<Pattern>,
}

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

struct Document<'a> {
    compiled: &'a Compiled,
    progress: Vec<usize>,
}

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
    use super::*;

    #[test]
    fn uncertain_or_oversized_query_state_passes() {
        let conn = Connection::open_in_memory().unwrap();
        assert!(install(&conn));
        for json in [
            "not json".to_owned(),
            "[]".into(),
            "[[]]".into(),
            "[[[]]]".into(),
            "[[[\"ab\"]]]".into(), // empty QUERY token stream
            serde_json::to_string(&vec![vec![vec!["a".repeat(MAX_TOKENS + 3)]]]).unwrap(),
            serde_json::to_string(&vec![vec![vec!["abc"]]; MAX_PHRASES + 1]).unwrap(),
        ] {
            assert!(
                conn.query_row(
                    "SELECT evorch_fts5_necessary('unrelated 日本語', ?1)",
                    [&json],
                    |row| row.get::<_, bool>(0),
                )
                .unwrap(),
                "{json:.100}"
            );
        }
        assert!(
            conn.query_row("SELECT evorch_fts5_necessary(NULL, NULL)", [], |row| {
                row.get::<_, bool>(0)
            })
            .unwrap()
        );
    }

    #[test]
    fn unknown_document_flags_abort_the_optimization_and_pass() {
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
        assert!(compiled.matches(&mut tokenizer, "abc").unwrap_or(true));
    }
}
