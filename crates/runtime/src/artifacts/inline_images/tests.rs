use std::path::PathBuf;

use super::*;
use crate::artifacts::{
    ArtifactRecord, ArtifactStore, CaptureRequest, MAX_HTML_BYTES, MAX_IMAGE_BYTES,
};

const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-inline-hero";
const OTHER_PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR-inline-decoy";
const GIF: &[u8] = b"GIF89a-inline-background";
const WEBP: &[u8] = b"RIFF\0\0\0\0WEBPVP8 inline-reference";

fn data_uri(media_type: &str, bytes: &[u8]) -> String {
    format!("data:{media_type};base64,{}", BASE64.encode(bytes))
}

fn request() -> CaptureRequest<'static> {
    CaptureRequest {
        title: "Mock",
        caption: None,
        run_id: "1".into(),
        root_run_id: "1".into(),
        thread_id: Some("thread".into()),
    }
}

/// A workspace, this thread's scratch, another thread's scratch beside it and a
/// directory outside every root (such as an allowed directory).
struct Fixture {
    _dirs: Vec<tempfile::TempDir>,
    workspace: PathBuf,
    scratch: PathBuf,
    other_thread: PathBuf,
    outside: PathBuf,
    store: ArtifactStore,
}

impl Fixture {
    fn new() -> Self {
        let dirs = (0..4)
            .map(|_| tempfile::tempdir().unwrap())
            .collect::<Vec<_>>();
        let canonical = |dir: &tempfile::TempDir| dir.path().canonicalize().unwrap();
        let workspace = canonical(&dirs[0]);
        let scratch = canonical(&dirs[1]).join("t-thread");
        let other_thread = canonical(&dirs[1]).join("t-other");
        std::fs::create_dir(&scratch).unwrap();
        std::fs::create_dir(&other_thread).unwrap();
        let outside = canonical(&dirs[2]);
        let store = ArtifactStore::new(dirs[3].path().join("artifacts")).unwrap();
        Self {
            _dirs: dirs,
            workspace,
            scratch,
            other_thread,
            outside,
            store,
        }
    }

    fn write(&self, path: &Path, bytes: impl AsRef<[u8]>) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn capture(&self, path: impl AsRef<Path>) -> Result<ArtifactRecord, String> {
        self.store.capture(
            &self.workspace,
            Some(&self.scratch),
            path.as_ref(),
            request(),
        )
    }

    fn stored(&self, record: &ArtifactRecord) -> Vec<u8> {
        std::fs::read(self.store.blob_path(record)).unwrap()
    }

    fn stored_count(&self) -> (usize, usize) {
        let count = |dir: &str| {
            std::fs::read_dir(self.store.root.join(dir))
                .unwrap()
                .count()
        };
        (count("meta"), count("blobs"))
    }
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn asset(path: &Path, bytes: &[u8]) -> InlinedAsset {
    InlinedAsset {
        source_path: path.to_string_lossy().into_owned(),
        sha256: sha256(bytes),
        byte_len: bytes.len() as u64,
    }
}

// Given: files in the workspace, this thread's scratch, another thread's scratch and
//        outside both / When: capturing each / Then: only the workspace and this
//        thread's scratch are sources, after resolving links and `..`
#[test]
fn capture_reads_the_workspace_and_this_threads_scratch_only() {
    let fixture = Fixture::new();
    let draft = fixture.scratch.join("draft.png");
    fixture.write(&fixture.workspace.join("mock.png"), PNG);
    fixture.write(&draft, PNG);
    fixture.write(&fixture.scratch.join("draft.html"), "<p>draft</p>");
    fixture.write(&fixture.other_thread.join("foreign.png"), PNG);
    fixture.write(&fixture.outside.join("allowed.png"), PNG);
    std::os::unix::fs::symlink(
        fixture.outside.join("allowed.png"),
        fixture.scratch.join("escape.png"),
    )
    .unwrap();
    std::fs::create_dir(fixture.scratch.join("sub")).unwrap();

    for accepted in [
        PathBuf::from("mock.png"),
        draft.clone(),
        fixture.scratch.join("draft.html"),
    ] {
        let record = fixture.capture(&accepted).unwrap();
        assert!(record.inlined_assets.is_empty(), "{}", accepted.display());
    }
    assert_eq!(
        fixture.capture(&draft).unwrap().source_path,
        draft.to_string_lossy()
    );
    let relative = fixture.capture("draft.png").unwrap_err();
    assert!(
        relative.contains("cannot open"),
        "relative paths resolve from the workspace: {relative}"
    );
    for rejected in [
        fixture.other_thread.join("foreign.png"),
        fixture.outside.join("allowed.png"),
        fixture.scratch.join("escape.png"),
        fixture.scratch.join("sub/../../t-other/foreign.png"),
    ] {
        let error = fixture.capture(&rejected).unwrap_err();
        assert!(
            error.contains("is outside this run's workspace and its $TMPDIR"),
            "{}: {error}",
            rejected.display()
        );
    }

    // A run without a scratch reads only its workspace.
    let error = fixture
        .store
        .capture(&fixture.workspace, None, &draft, request())
        .unwrap_err();
    assert!(
        error.ends_with("is outside this run's workspace"),
        "{error}"
    );
    // A scratch that no longer exists adds nothing and breaks nothing.
    let missing = fixture.scratch.join("missing-dir");
    assert!(
        fixture
            .store
            .capture(
                &fixture.workspace,
                Some(&missing),
                Path::new("mock.png"),
                request()
            )
            .is_ok()
    );
}

// Given: an HTML mock that references images by relative path from <img src>, a
//        <style> element and a style attribute, in every spelling a browser accepts
// When: capturing it directly and through a link / Then: each such reference becomes
//       a data URI resolved from the real file's directory, and nothing else changes
#[test]
fn html_images_are_inlined_from_the_html_files_directory() {
    let fixture = Fixture::new();
    let pages = fixture.workspace.join("pages");
    let hero = pages.join("img/hero.png");
    let background = fixture.workspace.join("shared/bg.gif");
    let reference = fixture.scratch.join("ref.webp");
    fixture.write(&hero, PNG);
    fixture.write(&background, GIF);
    fixture.write(&reference, WEBP);
    // The same relative path from the workspace root holds a different image.
    fixture.write(&fixture.workspace.join("img/hero.png"), OTHER_PNG);
    let template = |hero_css: &str,
                    reference_css: &str,
                    escaped_css: &str,
                    unquoted: &str,
                    query: &str,
                    attribute_css: &str,
                    with_srcset: &str| {
        format!(
            r#"<!doctype html>
<html><head>
<STYLE>
  .hero {{ background: {hero_css} no-repeat; }}
  /* url(missing-in-comment.png) */
  .quote::before {{ content: "url(missing-in-string.png)"; }}
  .ref {{ background-image: {reference_css}; }}
  .icon{{background:{escaped_css}}}
</STYLE>
</head><body>
<IMG SRC={unquoted} alt=hero>
<img src='{query}' src="repeated-attribute-is-ignored.png">
<div Style="color: red; background: {attribute_css}">x</div>
<!-- <img src="missing-in-comment.png"> -->
<script>document.write('<img src="missing-in-script.png">')</script>
<textarea><img src="missing-in-textarea.png"></textarea>
<img srcset="missing-srcset.png 2x" src="{with_srcset}">
<link rel=stylesheet href=missing.css>
</body></html>
"#
        )
    };
    let source = template(
        r#"URL( "img/hero.png" )"#,
        &format!("url({})", reference.display()),
        r"url(img\2f hero.png)",
        "img/hero.png",
        "img/hero.png?v=2#top",
        "url(&quot;../shared/bg.gif&quot;)",
        "img/hero.png",
    );
    let (png, gif, webp) = (
        data_uri("image/png", PNG),
        data_uri("image/gif", GIF),
        data_uri("image/webp", WEBP),
    );
    let expected = template(
        &format!("url({png})"),
        &format!("url({webp})"),
        &format!("url({png})"),
        &png,
        &png,
        &format!("url({gif})"),
        &png,
    );
    fixture.write(&pages.join("mock.html"), &source);
    std::os::unix::fs::symlink(pages.join("mock.html"), fixture.workspace.join("link.html"))
        .unwrap();

    let record = fixture.capture("pages/mock.html").unwrap();

    assert_eq!(
        String::from_utf8(fixture.stored(&record)).unwrap(),
        expected
    );
    assert_eq!(record.sha256, sha256(expected.as_bytes()));
    assert_eq!(record.byte_len, expected.len() as u64);
    assert_eq!(
        record.inlined_assets,
        [
            asset(&hero, PNG),
            asset(&reference, WEBP),
            asset(&background, GIF)
        ]
    );
    assert_eq!(fixture.store.load(&record.artifact_id).unwrap(), record);
    let linked = fixture.capture("link.html").unwrap();
    assert_eq!(
        linked.sha256, record.sha256,
        "links resolve from the real file"
    );
}

// Given: markup that browsers parse in less common ways / When: capturing / Then:
// references are found exactly where a browser would load them
#[test]
fn references_are_found_where_a_browser_parses_them() {
    let fixture = Fixture::new();
    let root = &fixture.workspace;
    fixture.write(&root.join("a.png"), PNG);
    let png = data_uri("image/png", PNG);
    let cases = [
        ("<img/src=a.png>", format!("<img/src={png}>")),
        (
            r#"<img src = "a.png" >"#,
            format!(r#"<img src = "{png}" >"#),
        ),
        (
            r#"<img SRC="a.png" Src="missing.png">"#,
            format!(r#"<img SRC="{png}" Src="missing.png">"#),
        ),
        (
            "<template><img src=a.png></template>",
            format!("<template><img src={png}></template>"),
        ),
        (
            "<svg><style>.a{fill:url(a&#46;png)}</style></svg>",
            format!("<svg><style>.a{{fill:url({png})}}</style></svg>"),
        ),
        (
            "<p style=background:url(a.png)>x</p>",
            format!("<p style=background:url({png})>x</p>"),
        ),
        ("<noscript><img src=missing.png></noscript>", String::new()),
        ("<title><img src=missing.png></title>", String::new()),
        ("<xmp><img src=missing.png></xmp>", String::new()),
        (
            "<style>/* <img src=missing.png> */ p{}</style>",
            String::new(),
        ),
    ];
    for (index, (source, expected)) in cases.into_iter().enumerate() {
        let name = format!("case-{index}.html");
        fixture.write(&root.join(&name), source);
        let record = fixture
            .capture(&name)
            .unwrap_or_else(|error| panic!("{source}: {error}"));
        // An empty expectation means the HTML is kept as written.
        let expected = if expected.is_empty() {
            source.to_owned()
        } else {
            expected
        };
        assert_eq!(
            String::from_utf8(fixture.stored(&record)).unwrap(),
            expected,
            "{source}"
        );
    }
}

// Given: references with a scheme, a host, only a fragment or no value / When:
// capturing / Then: the HTML is stored byte for byte as written
#[test]
fn references_with_a_scheme_a_host_or_only_a_fragment_are_kept() {
    let fixture = Fixture::new();
    let source = r##"<!doctype html>
<img src="https://example.com/a.png"><img src="HTTP://example.com/b.png">
<img src="data:image/png;base64,iVBORw0KGgo="><img src="file:///etc/c.png">
<img src="blob:https://example.com/d"><img src="//cdn.example/e.png">
<img src="#fragment"><img src=""><img src="   "><img src><img>
<a href="missing.png">link</a><video poster="missing.png"></video>
<style>
  .a { fill: url(#gradient); background: url(); mask: url(''); }
  .b { background: url( https://example.com/f.png ), url("data:image/gif;base64,R0lG"); }
</style>
<p style="background: url(http://example.com/g.png)">x</p>
"##;
    fixture.write(&fixture.workspace.join("remote.html"), source);

    let record = fixture.capture("remote.html").unwrap();

    assert_eq!(fixture.stored(&record), source.as_bytes());
    assert_eq!(record.sha256, sha256(source.as_bytes()));
    assert!(record.inlined_assets.is_empty());
}

// Given: one valid reference and references that are missing, outside the roots, in
//        another thread's scratch, mislabeled, not an image, too large, a directory
//        or a link out / When: capturing / Then: nothing is stored and the error
//        names each failing reference once, without any file content
#[test]
fn any_reference_that_cannot_be_inlined_rejects_the_whole_capture() {
    let fixture = Fixture::new();
    let root = &fixture.workspace;
    fixture.write(&root.join("img/ok.png"), PNG);
    fixture.write(&root.join("fake.png"), "SECRET-CONTENT is not an image");
    fixture.write(&root.join("font.woff2"), "wOF2");
    std::fs::File::create(root.join("big.png"))
        .unwrap()
        .set_len(MAX_IMAGE_BYTES + 1)
        .unwrap();
    std::fs::create_dir(root.join("folder.png")).unwrap();
    let allowed = fixture.outside.join("allowed.png");
    let foreign = fixture.other_thread.join("foreign.png");
    fixture.write(&allowed, PNG);
    fixture.write(&foreign, PNG);
    std::os::unix::fs::symlink(&allowed, root.join("escape.png")).unwrap();
    let source = format!(
        r#"<style>@font-face {{ font-family: x; src: url(font.woff2) format("woff2"); }}</style>
<img src="img/ok.png"><img src="missing.png"><img src="missing.png">
<img src="{}"><img src="{}"><img src="fake.png">
<p style="background: url(big.png)"></p><img src="folder.png"><img src="escape.png">"#,
        allowed.display(),
        foreign.display()
    );
    fixture.write(&root.join("mock.html"), source);

    let error = fixture.capture("mock.html").unwrap_err();

    let outside = "is outside this run's workspace and its $TMPDIR";
    for line in [
        "mock.html was not captured: 8 image references cannot be inlined".to_owned(),
        "\n- \"font.woff2\" is not a .png, .jpg, .gif or .webp file".into(),
        "\n- \"missing.png\" was not found".into(),
        format!("\n- {:?} {outside}", allowed.display().to_string()),
        format!("\n- {:?} {outside}", foreign.display().to_string()),
        "\n- \"fake.png\" does not contain valid image/png data".into(),
        "\n- \"big.png\" exceeds the 8 MiB limit for image/png".into(),
        "\n- \"folder.png\" is not a regular file".into(),
        format!("\n- \"escape.png\" {outside}"),
    ] {
        assert!(error.contains(&line), "missing {line:?} in {error}");
    }
    assert_eq!(error.matches("missing.png").count(), 1, "{error}");
    assert!(!error.contains("ok.png"), "{error}");
    assert!(!error.contains("SECRET-CONTENT"), "{error}");
    assert_eq!(fixture.stored_count(), (0, 0));
}

// Given: more failing references than a rejection lists, the first one very long /
// When: capturing / Then: the first ones are quoted, shortened, and the rest counted
#[test]
fn a_rejection_lists_the_first_failing_references_and_counts_the_rest() {
    let fixture = Fixture::new();
    let long = "long".repeat(60);
    let source = format!("<img src=\"{long}.png\">")
        + &(0..11)
            .map(|index| format!("<img src=\"missing-{index}.png\">"))
            .collect::<String>();
    fixture.write(&fixture.workspace.join("mock.html"), source);

    let error = fixture.capture("mock.html").unwrap_err();

    assert!(error.contains("12 image references"), "{error}");
    let shortened = format!("\n- \"{}…\" was not found", "long".repeat(50));
    assert!(error.contains(&shortened), "{error}");
    assert_eq!(
        error.matches("\n- \"missing-").count(),
        MAX_LISTED - 1,
        "{error}"
    );
    assert!(error.contains("\n- and 4 more"), "{error}");
    assert!(!error.contains("missing-7.png"), "{error}");
}

/// A PNG of `len` bytes.
fn large_png(len: usize) -> Vec<u8> {
    let mut bytes = PNG.to_vec();
    bytes.resize(len, 0);
    bytes
}

// Given: images that fit the 8 MiB stored-HTML limit once but not twice / When:
// capturing / Then: the HTML may exceed 2 MiB once inlined, and the over-limit
// captures are rejected without storing anything
#[test]
fn inlined_html_must_fit_the_bundled_limit() {
    let fixture = Fixture::new();
    let root = &fixture.workspace;
    let image = large_png(3_500_000);
    fixture.write(&root.join("a.png"), &image);
    fixture.write(&root.join("b.png"), &image[..3_400_000]);
    fixture.write(&root.join("once.html"), "<img src=a.png>");
    fixture.write(&root.join("twice.html"), "<img src=a.png><img src=a.png>");
    fixture.write(&root.join("both.html"), "<img src=a.png><img src=b.png>");

    let once = fixture.capture("once.html").unwrap();
    assert!(once.byte_len > MAX_HTML_BYTES, "{}", once.byte_len);
    assert!(once.byte_len <= MAX_BUNDLED_HTML_BYTES);
    let stored = fixture.stored_count();
    for over in ["twice.html", "both.html"] {
        let error = fixture.capture(over).unwrap_err();
        assert!(
            error.contains("over the 8 MiB limit for a captured HTML file"),
            "{over}: {error}"
        );
    }
    assert_eq!(fixture.stored_count(), stored);
}

// Given: an image whose base64 looks like an API key, and HTML that holds one / When:
// capturing / Then: the secret guard checks only the HTML as written
#[test]
fn the_secret_guard_checks_the_html_as_written() {
    let fixture = Fixture::new();
    let root = &fixture.workspace;
    // 9 bytes encode to 12 characters, so the decoded text below is encoded as is.
    let mut image = large_png(9);
    image.extend(
        BASE64
            .decode(format!("AAA/AIza{}", "B".repeat(32)))
            .unwrap(),
    );
    fixture.write(&root.join("key-like.png"), &image);
    fixture.write(&root.join("mock.html"), "<img src=key-like.png>");
    let guard = secret_guard::SecretRedactor::from_env();

    let record = fixture.capture("mock.html").unwrap();
    let stored = String::from_utf8(fixture.stored(&record)).unwrap();
    assert!(guard.detect(&stored).is_some(), "{stored}");

    fixture.write(
        &root.join("leak.html"),
        format!("<img src=key-like.png><p>sk-{}</p>", "a".repeat(32)),
    );
    let error = fixture.capture("leak.html").unwrap_err();
    assert!(error.contains("credential"), "{error}");
}

// Given: a capture without inlined images and metadata written before inlining
// existed / When: storing and loading / Then: neither carries the new field
#[test]
fn metadata_without_inlined_assets_keeps_its_previous_form() {
    let fixture = Fixture::new();
    fixture.write(&fixture.workspace.join("mock.png"), PNG);
    let record = fixture.capture("mock.png").unwrap();
    let meta = std::fs::read_to_string(
        fixture
            .store
            .root
            .join("meta")
            .join(format!("{}.json", record.artifact_id)),
    )
    .unwrap();
    assert!(!meta.contains("inlined_assets"), "{meta}");

    let id = "artifact-0123456789abcdef0123456789abcdef";
    std::fs::write(
        fixture.store.root.join("meta").join(format!("{id}.json")),
        format!(
            r#"{{"artifact_id":"{id}","sha256":"{}","media_type":"text/html","extension":"html","title":"Old","source_path":"/w/mock.html","byte_len":11,"run_id":"1","root_run_id":"1","thread_id":"thread"}}"#,
            sha256(b"<p>old</p>\n")
        ),
    )
    .unwrap();
    let old = fixture.store.load(id).unwrap();
    assert_eq!(old.title, "Old");
    assert!(old.inlined_assets.is_empty());
}

// --- scanning ----------------------------------------------------------------

#[test]
fn css_url_tokens_follow_the_css_tokenizer() {
    let found = |css: &str, expected: &[(&str, &str)]| {
        let actual = css_urls(css)
            .into_iter()
            .map(|(range, value)| (&css[range], value))
            .collect::<Vec<_>>();
        let expected = expected
            .iter()
            .map(|(token, value)| (*token, (*value).to_owned()))
            .collect::<Vec<_>>();
        assert_eq!(actual, expected, "{css:?}");
    };
    found("a{b:url(x.png)}", &[("url(x.png)", "x.png")]);
    found("b:URL( 'y.png' ) ", &[("URL( 'y.png' )", "y.png")]);
    found(r#"url("a\"b.png")"#, &[(r#"url("a\"b.png")"#, "a\"b.png")]);
    found(r"url(a\ b.png)", &[(r"url(a\ b.png)", "a b.png")]);
    found(r"url(\61 .png)", &[(r"url(\61 .png)", "a.png")]);
    found(
        "url(\"multi\\\nline.png\")",
        &[("url(\"multi\\\nline.png\")", "multiline.png")],
    );
    found(
        "/* url(no.png) */ url(yes.png)",
        &[("url(yes.png)", "yes.png")],
    );
    found(r#"content:"url(no.png)"; x:'url(no)'"#, &[]);
    found(r#"\"url(no.png) x\url(no.png)"#, &[]);
    found("myurl(no.png) -url(no.png) url-x(no.png)", &[]);
    found("url(a b.png) url(ok.png)", &[("url(ok.png)", "ok.png")]);
    found(r#"url(a"b.png) url(ok.png)"#, &[("url(ok.png)", "ok.png")]);
    found("url(\"cut\n) url(ok.png)", &[("url(ok.png)", "ok.png")]);
    found(
        r#"url("x.png" junk) url(ok.png)"#,
        &[("url(ok.png)", "ok.png")],
    );
    found("url(eof.png", &[("url(eof.png", "eof.png")]);
    found(
        "a:url(x.png),url( \"y.png\" )",
        &[("url(x.png)", "x.png"), ("url( \"y.png\" )", "y.png")],
    );
    found(
        "url(日本.png) url(é.png)",
        &[("url(日本.png)", "日本.png"), ("url(é.png)", "é.png")],
    );
}

#[test]
fn character_references_map_back_to_their_raw_span() {
    let raw = "a&quot;b&#x41;&#66&amp;&unknown;&";
    let decoded = Decoded::new(raw, true);
    assert_eq!(decoded.text, "a\"bAB&&unknown;&");
    let position = decoded.text.find('A').unwrap();
    assert_eq!(&raw[decoded.raw_range(position..position + 1)], "&#x41;");
    assert_eq!(&raw[decoded.raw_range(0..decoded.text.len())], raw);
    let plain = Decoded::new("a&quot;", false);
    assert_eq!(plain.text, "a&quot;");
    assert_eq!(plain.raw_range(1..3), 1..3);
}

#[test]
fn local_references_resolve_like_a_browser_from_the_html_directory() {
    let base = Path::new("/w/pages");
    for (url, expected) in [
        ("img/a.png?x=1#y", "/w/pages/img/a.png"),
        ("../shared/b%20c.gif", "/w/shared/b c.gif"),
        ("./x/./y.png", "/w/pages/x/y.png"),
        ("img\\a.png", "/w/pages/img/a.png"),
        ("/abs/z.png", "/abs/z.png"),
        ("../../../../up.png", "/up.png"),
        ("%2e%2e/dot.png", "/w/dot.png"),
        ("bad%zz.png", "/w/pages/bad%zz.png"),
    ] {
        assert_eq!(local_path(base, url), Path::new(expected), "{url}");
    }
    for local in ["a.png", "./a.png", "../a.png", "/a.png", "img/x:y.png"] {
        assert!(is_local(local), "{local}");
    }
    for kept in [
        "",
        "#x",
        "//host/x.png",
        "\\\\host\\x.png",
        "http://h/x.png",
        "HTTPS://h/x.png",
        "data:image/png;base64,AA",
        "file:///x.png",
        "blob:x",
        "c:\\x.png",
    ] {
        assert!(!is_local(kept), "{kept}");
    }
    assert_eq!(browser_url("  \t img/\na.png \n"), "img/a.png");
}
