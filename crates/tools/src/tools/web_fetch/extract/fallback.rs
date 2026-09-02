use scraper::Html;

use super::{Extracted, Extractor, OutputFormat};

pub(super) struct FallbackStage;

impl FallbackStage {
    pub(super) fn extract_content(html: &str, format: OutputFormat) -> Extracted {
        let content = match format {
            OutputFormat::Text => Html::parse_document(html)
                .root_element()
                .text()
                .collect::<String>(),
            OutputFormat::Markdown => htmd::convert(html).unwrap_or_default(),
            OutputFormat::Html => html.to_string(),
        };
        Extracted {
            content,
            method: "fallback",
        }
    }
}

impl Extractor for FallbackStage {
    fn name(&self) -> &'static str {
        "fallback"
    }

    fn extract(&self, html: &str, _url: &str, format: OutputFormat) -> Option<Extracted> {
        Some(Self::extract_content(html, format))
    }
}
