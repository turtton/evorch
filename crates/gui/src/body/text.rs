//! Text that may still be in the store: inline pieces and unresolved references,
//! in order. A reference is never joined into a string; only
//! [`super::BodyResolver`] turns it into text, and only after it was read.

use std::fmt;

use super::types::DeferredBody;

/// One piece of a body.
#[derive(Clone, PartialEq)]
pub enum Segment {
    Inline(String),
    Deferred(DeferredBody),
}

impl fmt::Debug for Segment {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inline(text) => write!(f, "{text:?}"),
            Self::Deferred(body) => write!(f, "<deferred {:?}>", body.key),
        }
    }
}

/// A body made of [`Segment`]s. Adjacent inline pieces are always merged, and
/// a body with no deferred piece holds at most one segment.
#[derive(Clone, Default, PartialEq)]
pub struct BodyText {
    segments: Vec<Segment>,
}

impl fmt::Debug for BodyText {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.segments.as_slice() {
            [] => f.write_str("\"\""),
            [Segment::Inline(text)] => write!(f, "{text:?}"),
            segments => f.debug_list().entries(segments).finish(),
        }
    }
}

impl BodyText {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    #[must_use]
    pub fn deferred(body: DeferredBody) -> Self {
        Self {
            segments: vec![Segment::Deferred(body)],
        }
    }

    #[must_use]
    pub fn segments(&self) -> &[Segment] {
        &self.segments
    }

    #[must_use]
    pub fn has_deferred(&self) -> bool {
        self.segments
            .iter()
            .any(|segment| matches!(segment, Segment::Deferred(_)))
    }

    /// The deferred pieces, in order.
    pub fn bodies(&self) -> impl Iterator<Item = &DeferredBody> {
        self.segments.iter().filter_map(|segment| match segment {
            Segment::Deferred(body) => Some(body),
            Segment::Inline(_) => None,
        })
    }

    /// The text when nothing in it is still in the store.
    #[must_use]
    pub fn inline(&self) -> Option<&str> {
        match self.segments.as_slice() {
            [] => Some(""),
            [Segment::Inline(text)] => Some(text),
            _ => None,
        }
    }

    /// The inline text, or an empty string while a piece is in the store.
    /// Only for code that has nothing to show before the body is read.
    #[must_use]
    pub fn inline_or_empty(&self) -> &str {
        self.inline().unwrap_or("")
    }

    /// The size in bytes: inline text as is, deferred pieces as declared.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.segments
            .iter()
            .map(|segment| match segment {
                Segment::Inline(text) => text.len() as u64,
                Segment::Deferred(body) => body.key.byte_len,
            })
            .sum()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        usize::try_from(self.byte_len()).unwrap_or(usize::MAX)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.byte_len() == 0
    }

    pub fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.segments.last_mut() {
            Some(Segment::Inline(last)) => last.push_str(text),
            _ => self.segments.push(Segment::Inline(text.to_owned())),
        }
    }

    pub fn push_body(&mut self, other: Self) {
        for segment in other.segments {
            match segment {
                Segment::Inline(text) => self.push_str(&text),
                Segment::Deferred(body) => self.segments.push(Segment::Deferred(body)),
            }
        }
    }
}

impl From<&str> for BodyText {
    fn from(text: &str) -> Self {
        let mut body = Self::new();
        body.push_str(text);
        body
    }
}

impl From<String> for BodyText {
    fn from(text: String) -> Self {
        if text.is_empty() {
            return Self::new();
        }
        Self {
            segments: vec![Segment::Inline(text)],
        }
    }
}

impl From<&String> for BodyText {
    fn from(text: &String) -> Self {
        text.as_str().into()
    }
}

impl PartialEq<str> for BodyText {
    fn eq(&self, other: &str) -> bool {
        self.inline() == Some(other)
    }
}

impl PartialEq<&str> for BodyText {
    fn eq(&self, other: &&str) -> bool {
        self.inline() == Some(*other)
    }
}

impl PartialEq<String> for BodyText {
    fn eq(&self, other: &String) -> bool {
        self.inline() == Some(other.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::body::BodyRef;

    fn deferred(len: u64) -> DeferredBody {
        DeferredBody::new(
            BodyRef {
                hash: [len as u8; 32],
                byte_len: len,
            },
            Some("run"),
            Some(1),
        )
    }

    #[test]
    fn inline_pieces_merge_and_deferred_pieces_keep_their_order() {
        let mut body = BodyText::from("a");
        body.push_str("b");
        assert_eq!(body, "ab");
        body.push_body(BodyText::deferred(deferred(5000)));
        body.push_str("c");
        body.push_str("d");
        assert_eq!(body.segments().len(), 3);
        assert_eq!(body.byte_len(), 2 + 5000 + 2);
        assert!(body.has_deferred());
        // Text that is partly in the store never equals a plain string.
        assert!(body != "abcd");
        assert_eq!(body.inline(), None);
    }

    #[test]
    fn an_empty_body_has_no_segments() {
        assert!(BodyText::from("").segments().is_empty());
        assert!(BodyText::new().is_empty());
        assert_eq!(BodyText::new(), "");
    }
}
