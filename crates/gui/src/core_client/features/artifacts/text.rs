//! The text or HTML source of an artifact, read in the core's UTF-8 chunks.
//!
//! The core cuts every chunk on a character boundary and answers `offset` with the
//! byte the chunk starts at, so the next request starts at the number of bytes
//! already held. A reply that does not start there, or that makes no progress, is
//! refused instead of guessed at. Only [`MAX_TEXT_BYTES`] (plus one chunk) is kept.
use std::ops::Range;
use workbench_protocol::namespaces::artifacts::MAX_CONTENT_CHUNK_BYTES;

/// One request asks for the largest chunk the protocol allows.
pub(super) const CHUNK_BYTES: u32 = MAX_CONTENT_CHUNK_BYTES as u32;
/// Reading stops once this much is held; a chunk may overshoot by less than one chunk.
pub(super) const MAX_TEXT_BYTES: usize = 256 * 1024;
/// A line longer than this many characters is cut for display (never inside a character).
pub(super) const MAX_LINE_CHARS: usize = 2000;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum AppendError {
    /// The chunk does not start where the held text ends.
    Gap,
    /// An empty chunk that is not the end.
    NoProgress,
}

#[derive(Default)]
pub(super) struct TextBody {
    text: String,
    /// Byte ranges of the lines of `text`, the last one possibly still growing.
    lines: Vec<Range<usize>>,
    eof: bool,
}

impl TextBody {
    pub(super) fn len(&self) -> usize {
        self.text.len()
    }

    /// More is to be asked for.
    pub(super) fn wants_more(&self) -> bool {
        !self.eof && self.text.len() < MAX_TEXT_BYTES
    }

    /// The end of the artifact was not reached: the viewer's limit stopped the read.
    pub(super) fn capped(&self) -> bool {
        !self.eof && self.text.len() >= MAX_TEXT_BYTES
    }

    pub(super) fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// The line `index` for display, and whether it was cut.
    pub(super) fn line(&self, index: usize) -> (&str, bool) {
        let line = self.lines.get(index).map_or("", |r| &self.text[r.clone()]);
        match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((end, _)) => (&line[..end], true),
            None => (line, false),
        }
    }

    pub(super) fn append(
        &mut self,
        offset: u64,
        chunk: &str,
        eof: bool,
    ) -> Result<(), AppendError> {
        if offset != self.text.len() as u64 {
            return Err(AppendError::Gap);
        }
        if chunk.is_empty() && !eof {
            return Err(AppendError::NoProgress);
        }
        self.text.push_str(chunk);
        self.eof = eof;
        let from = self.lines.pop().map_or(0, |last| last.start);
        let mut start = from;
        for (at, byte) in self.text.as_bytes()[from..].iter().enumerate() {
            if *byte == b'\n' {
                self.lines.push(start..from + at);
                start = from + at + 1;
            }
        }
        self.lines.push(start..self.text.len());
        Ok(())
    }
}
