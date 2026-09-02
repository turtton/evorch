use dom_smoothie::{Config, Readability, TextMode};

use super::{Extracted, Extractor, OutputFormat};

pub(super) struct ReadabilityStage;

impl Extractor for ReadabilityStage {
    fn name(&self) -> &'static str {
        "readability"
    }

    fn extract(&self, html: &str, url: &str, format: OutputFormat) -> Option<Extracted> {
        let text_mode = match format {
            OutputFormat::Text => TextMode::Formatted,
            OutputFormat::Markdown => TextMode::Markdown,
            OutputFormat::Html => return None,
        };
        let config = Config {
            text_mode,
            ..Config::default()
        };
        let mut readability = Readability::new(html, Some(url), Some(config)).ok()?;
        if !readability.is_probably_readable() {
            return None;
        }
        let article = readability.parse().ok()?;
        let content = article.text_content.to_string();
        if content.trim().is_empty() {
            return None;
        }
        Some(Extracted {
            content,
            method: self.name(),
        })
    }
}
