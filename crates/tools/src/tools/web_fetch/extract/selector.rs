use scraper::{Html, Selector};

use super::{Extracted, Extractor, OutputFormat};

pub(super) struct SelectorStage {
    selector: String,
}

impl SelectorStage {
    pub(super) fn new(selector: &str) -> Self {
        Self {
            selector: selector.to_string(),
        }
    }
}

impl Extractor for SelectorStage {
    fn name(&self) -> &'static str {
        "selector"
    }

    fn extract(&self, html: &str, _url: &str, format: OutputFormat) -> Option<Extracted> {
        let document = Html::parse_document(html);
        let selector = Selector::parse(&self.selector).ok()?;
        let content = match format {
            OutputFormat::Text => document
                .select(&selector)
                .map(|element| element.text().collect::<String>())
                .collect::<Vec<_>>()
                .join("\n"),
            OutputFormat::Markdown => {
                let selected_html = document
                    .select(&selector)
                    .map(|element| element.html())
                    .collect::<String>();
                htmd::convert(&selected_html).ok()?
            }
            OutputFormat::Html => return None,
        };
        if content.trim().is_empty() {
            return None;
        }
        Some(Extracted {
            content,
            method: self.name(),
        })
    }
}
