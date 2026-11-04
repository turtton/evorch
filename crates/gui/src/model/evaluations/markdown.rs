//! `report.md` is shown with the shared Markdown renderer, which opens links
//! and loads images. A report is data an evaluation produced, so those are
//! turned into plain text before it is displayed: reading a report never
//! reaches outside the machine.

use pulldown_cmark::{Event, Parser, Tag, TagEnd};

/// `source` with every link and image replaced by its text. A link keeps its
/// destination as inline code so the reader still sees where it pointed.
pub fn neutralize(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let mut copied = 0;
    let mut open: Option<Open> = None;
    let mut depth = 0usize;
    for (event, range) in Parser::new(source).into_offset_iter() {
        match event {
            Event::Start(Tag::Link { dest_url, .. }) => {
                depth += 1;
                if depth == 1 {
                    open = Some(Open {
                        start: range.start,
                        image: false,
                        destination: dest_url.to_string(),
                        text: String::new(),
                    });
                }
            }
            Event::Start(Tag::Image { dest_url, .. }) => {
                depth += 1;
                if depth == 1 {
                    open = Some(Open {
                        start: range.start,
                        image: true,
                        destination: dest_url.to_string(),
                        text: String::new(),
                    });
                }
            }
            Event::End(TagEnd::Link | TagEnd::Image) => {
                depth = depth.saturating_sub(1);
                if depth == 0
                    && let Some(link) = open.take()
                    && link.start >= copied
                {
                    out.push_str(&source[copied..link.start]);
                    out.push_str(&link.replacement());
                    copied = range.end;
                }
            }
            Event::Text(text) | Event::Code(text) => {
                if let Some(link) = open.as_mut() {
                    link.text.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if let Some(link) = open.as_mut() {
                    link.text.push(' ');
                }
            }
            _ => {}
        }
    }
    out.push_str(&source[copied..]);
    out
}

struct Open {
    start: usize,
    image: bool,
    destination: String,
    text: String,
}

impl Open {
    fn replacement(&self) -> String {
        let text = escape(self.text.trim());
        let destination = self.destination.replace(['`', '\n', '\r'], "");
        match (self.image, text.is_empty(), destination.is_empty()) {
            (true, true, _) => "(image)".to_owned(),
            (true, false, _) => format!("(image: {text})"),
            (false, _, true) => text,
            (false, true, false) => format!("`{destination}`"),
            (false, false, false) => format!("{text} (`{destination}`)"),
        }
    }
}

/// Keeps text that was link text from forming a link or image again.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if matches!(character, '[' | ']' | '<' | '>' | '!' | '`' | '\\') {
            out.push('\\');
        }
        out.push(character);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn has_link_or_image(markdown: &str) -> bool {
        Parser::new(markdown)
            .any(|event| matches!(event, Event::Start(Tag::Link { .. } | Tag::Image { .. })))
    }

    #[test]
    fn links_and_images_become_text_that_cannot_be_followed() {
        let source = "# Report\n\nSee [the docs](https://example.com/docs) and ![chart](https://example.com/c.png).\n\n<https://example.com/auto>\n\n[![badge](https://example.com/b.svg)](https://example.com/build)\n";
        let shown = neutralize(source);
        assert!(!has_link_or_image(&shown), "{shown}");
        assert!(shown.contains("# Report"));
        assert!(shown.contains("the docs (`https://example.com/docs`)"));
        assert!(shown.contains("(image: chart)"));
        assert!(shown.contains("`https://example.com/auto`"));
    }

    #[test]
    fn plain_markdown_is_unchanged() {
        let source =
            "- one\n- two\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n```rust\nlet x = [1](2);\n```\n";
        assert_eq!(neutralize(source), source);
    }

    #[test]
    fn link_text_cannot_rebuild_a_link() {
        let shown = neutralize("[\\[x\\](https://evil.example)](https://example.com)");
        assert!(!has_link_or_image(&shown), "{shown}");
    }
}
