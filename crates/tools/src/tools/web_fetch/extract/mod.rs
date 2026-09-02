//! `web_fetch` extraction chain extension point; prepend site-aware extractors with [`ExtractionChain::with_prepended`].

use super::OutputFormat;

mod fallback;
mod readability;
mod selector;

use fallback::FallbackStage;
use readability::ReadabilityStage;
use selector::SelectorStage;

pub struct Extracted {
    pub content: String,
    pub method: &'static str,
}

pub trait Extractor: Send + Sync {
    fn name(&self) -> &'static str;
    fn extract(&self, html: &str, url: &str, format: OutputFormat) -> Option<Extracted>;
}

pub struct ExtractionChain {
    extractors: Vec<Box<dyn Extractor>>,
}

impl ExtractionChain {
    pub fn standard(selector: Option<&str>) -> Self {
        let mut extractors: Vec<Box<dyn Extractor>> = Vec::with_capacity(3);
        if let Some(selector) = selector {
            extractors.push(Box::new(SelectorStage::new(selector)));
        }
        extractors.push(Box::new(ReadabilityStage));
        extractors.push(Box::new(FallbackStage));
        Self { extractors }
    }

    pub fn with_prepended(mut self, extractor: Box<dyn Extractor>) -> Self {
        self.extractors.insert(0, extractor);
        self
    }

    pub fn run(&self, html: &str, url: &str, format: OutputFormat) -> Extracted {
        for extractor in &self.extractors {
            if let Some(extracted) = extractor.extract(html, url, format) {
                return extracted;
            }
        }
        FallbackStage::extract_content(html, format)
    }
}

#[cfg(test)]
mod tests;
