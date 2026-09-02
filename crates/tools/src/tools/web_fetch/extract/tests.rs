use super::*;

struct FixedExtractor {
    method: &'static str,
    content: Option<&'static str>,
}

impl Extractor for FixedExtractor {
    fn name(&self) -> &'static str {
        self.method
    }

    fn extract(&self, _html: &str, _url: &str, _format: OutputFormat) -> Option<Extracted> {
        self.content.map(|content| Extracted {
            content: content.to_string(),
            method: self.name(),
        })
    }
}

const ARTICLE_HTML: &str = r#"
    <html><body>
      <nav><a href='/home'>Home</a><a href='/products'>Products</a></nav>
      <article>
        <h1>Reliable extraction</h1>
        <p>This article contains a deliberately substantial paragraph so the readability heuristic can identify it as meaningful editorial content rather than navigation or incidental page chrome. It explains how the extractor preserves the central story while ignoring menus, advertisements, and unrelated links. The paragraph continues with enough concrete prose to exceed the library threshold and make this fixture deterministic across runs. Readers receive the useful explanation, examples, and conclusions without having to sift through repetitive controls or promotional material surrounding the document.</p>
        <ul><li>First result</li><li>Second result</li></ul>
      </article>
      <footer>Copyright noise</footer>
    </body></html>
"#;

#[test]
fn selector_stage_extracts_matched_text() {
    // Given
    let html = "<main><p class='keep'>First <strong>item</strong></p><p class='keep'>Second</p><p>Ignored</p></main>";
    let stage = SelectorStage::new(".keep");
    // When
    let extracted = stage
        .extract(html, "https://example.com/page", OutputFormat::Text)
        .expect("selector should match");
    // Then
    assert_eq!(extracted.content, "First item\nSecond");
    assert_eq!(extracted.method, "selector");
}

#[test]
fn selector_stage_markdown_converts_selected_html() {
    // Given
    let html = "<main><section class='keep'><h2>Title</h2><p>A <strong>bold</strong> point.</p></section><aside>Ignored</aside></main>";
    let stage = SelectorStage::new(".keep");
    // When
    let extracted = stage
        .extract(html, "https://example.com/page", OutputFormat::Markdown)
        .expect("selector should match");
    // Then
    assert!(extracted.content.contains("## Title"));
    assert!(extracted.content.contains("**bold**"));
    assert!(!extracted.content.contains("Ignored"));
}

#[test]
fn selector_invalid_returns_none_defensive() {
    // Given
    let stage = SelectorStage::new("[");
    // When
    let extracted = stage.extract(
        "<main>content</main>",
        "https://example.com/page",
        OutputFormat::Text,
    );
    // Then
    assert!(extracted.is_none());
}

#[test]
fn readability_stage_extracts_main_content() {
    // Given
    let stage = ReadabilityStage;
    // When
    let extracted = stage
        .extract(
            ARTICLE_HTML,
            "https://example.com/article",
            OutputFormat::Text,
        )
        .expect("article should be readable");
    // Then
    assert!(extracted.content.contains("Reliable extraction"));
    assert!(extracted.content.contains("meaningful editorial content"));
    assert!(!extracted.content.contains("Products"));
    assert_eq!(extracted.method, "readability");
}

#[test]
fn readability_markdown_mode_outputs_markdown() {
    // Given
    let stage = ReadabilityStage;
    // When
    let extracted = stage
        .extract(
            ARTICLE_HTML,
            "https://example.com/article",
            OutputFormat::Markdown,
        )
        .expect("article should be readable");
    // Then
    assert!(extracted.content.contains("# Reliable extraction"));
    assert!(extracted.content.contains("- First result"));
}

#[test]
fn selector_no_match_falls_through_to_readability() {
    // Given
    let chain = ExtractionChain::standard(Some(".missing"));
    // When
    let extracted = chain.run(
        ARTICLE_HTML,
        "https://example.com/article",
        OutputFormat::Text,
    );
    // Then
    assert_eq!(extracted.method, "readability");
    assert!(extracted.content.contains("meaningful editorial content"));
}

#[test]
fn unreadable_document_falls_back_to_full_text() {
    // Given
    let html = "<html><body><nav>Home Short menu</nav><div>Tiny page</div></body></html>";
    let chain = ExtractionChain::standard(None);
    // When
    let extracted = chain.run(html, "https://example.com/menu", OutputFormat::Text);
    // Then
    assert_eq!(extracted.method, "fallback");
    assert!(extracted.content.contains("Home Short menu"));
    assert!(extracted.content.contains("Tiny page"));
}

#[test]
fn fallback_markdown_uses_htmd_full_document() {
    // Given
    let html = "<html><body><h1>Whole page</h1><ul><li>Alpha</li><li>Beta</li></ul></body></html>";
    let stage = FallbackStage;
    // When
    let extracted = stage
        .extract(html, "https://example.com/page", OutputFormat::Markdown)
        .expect("fallback is infallible");
    // Then
    assert!(extracted.content.contains("# Whole page"));
    assert!(extracted.content.contains("*"));
    assert!(extracted.content.contains("Alpha"));
    assert_eq!(extracted.method, "fallback");
}

#[test]
fn prepended_extractor_takes_priority() {
    // Given
    let chain =
        ExtractionChain::standard(Some("article")).with_prepended(Box::new(FixedExtractor {
            method: "site-aware",
            content: Some("priority content"),
        }));
    // When
    let extracted = chain.run(
        ARTICLE_HTML,
        "https://example.com/article",
        OutputFormat::Text,
    );
    // Then
    assert_eq!(extracted.content, "priority content");
    assert_eq!(extracted.method, "site-aware");
}

#[test]
fn chain_first_some_wins_order() {
    // Given
    let chain = ExtractionChain {
        extractors: vec![
            Box::new(FixedExtractor {
                method: "none",
                content: None,
            }),
            Box::new(FixedExtractor {
                method: "first-some",
                content: Some("first"),
            }),
            Box::new(FixedExtractor {
                method: "later-some",
                content: Some("later"),
            }),
        ],
    };
    // When
    let extracted = chain.run("", "https://example.com", OutputFormat::Text);
    // Then
    assert_eq!(extracted.content, "first");
    assert_eq!(extracted.method, "first-some");
}
