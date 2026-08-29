#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn feed_splits_lines_and_reassembles_utf8() {
        let mut buffer = TerminalBuffer::new(10);
        buffer.feed("a\n".as_bytes());
        let bytes = "日".as_bytes();
        buffer.feed(&bytes[..1]);
        buffer.feed(&bytes[1..]);
        assert_eq!(buffer.lines(), &["a", "日"]);
    }

    #[test]
    fn carriage_return_overwrites_current_line() {
        let mut buffer = TerminalBuffer::new(10);
        buffer.feed(b"abc\rxy");
        assert_eq!(buffer.lines(), &["xyc"]);
    }

    #[test]
    fn csi_and_osc_sequences_are_stripped() {
        let mut buffer = TerminalBuffer::new(10);
        buffer.feed(b"a\x1b[31mb\x1b[0m\x1b]title\x07c");
        assert_eq!(buffer.lines(), &["abc"]);
    }

    #[test]
    fn scrollback_is_capped() {
        let mut buffer = TerminalBuffer::new(2);
        buffer.feed(b"one\ntwo\nthree");
        assert_eq!(buffer.lines(), &["two", "three"]);
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EscapeState {
    Normal,
    Escape,
    Csi,
    Osc,
    OscEscape,
}

#[derive(Debug, Clone)]
pub struct TerminalBuffer {
    lines: Vec<String>,
    current: Vec<char>,
    max_lines: usize,
    utf8_carry: Vec<u8>,
    escape: EscapeState,
    cursor: usize,
}

impl TerminalBuffer {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: Vec::new(),
            current: Vec::new(),
            max_lines,
            utf8_carry: Vec::new(),
            escape: EscapeState::Normal,
            cursor: 0,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.utf8_carry.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.utf8_carry) {
                Ok(text) => {
                    let text = text.to_owned();
                    self.utf8_carry.clear();
                    self.consume(text.chars());
                    break;
                }
                Err(error) => {
                    let valid = error.valid_up_to();
                    if error.error_len().is_none() {
                        let tail = self.utf8_carry.split_off(valid);
                        let text = String::from_utf8_lossy(&self.utf8_carry).into_owned();
                        self.utf8_carry = tail;
                        self.consume(text.chars());
                        break;
                    }
                    let text = String::from_utf8_lossy(&self.utf8_carry[..valid + 1]).into_owned();
                    self.utf8_carry.drain(..valid + 1);
                    self.consume(text.chars());
                }
            }
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        lines.push(self.current.iter().collect());
        lines
    }

    fn consume(&mut self, chars: impl Iterator<Item = char>) {
        for ch in chars {
            match self.escape {
                EscapeState::Normal => match ch {
                    '\x1b' => self.escape = EscapeState::Escape,
                    '\n' => self.newline(),
                    '\r' => self.cursor = 0,
                    '\x08' => {
                        if self.cursor > 0 {
                            self.cursor -= 1;
                            self.current.remove(self.cursor);
                        }
                    }
                    '\t' => {
                        for _ in 0..4 {
                            self.put(' ');
                        }
                    }
                    _ => self.put(ch),
                },
                EscapeState::Escape => {
                    self.escape = match ch {
                        '[' => EscapeState::Csi,
                        ']' => EscapeState::Osc,
                        _ => EscapeState::Normal,
                    }
                }
                EscapeState::Csi => {
                    if ch == '[' {
                        self.escape = EscapeState::Csi
                    } else if ('@'..='~').contains(&ch) {
                        self.escape = EscapeState::Normal
                    }
                }
                EscapeState::Osc => {
                    if ch == '\x07' {
                        self.escape = EscapeState::Normal
                    } else if ch == '\x1b' {
                        self.escape = EscapeState::OscEscape
                    }
                }
                EscapeState::OscEscape => {
                    self.escape = if ch == '\\' {
                        EscapeState::Normal
                    } else {
                        EscapeState::Osc
                    }
                }
            }
        }
    }

    fn put(&mut self, ch: char) {
        if self.cursor == self.current.len() {
            self.current.push(ch);
        } else {
            self.current[self.cursor] = ch;
        }
        self.cursor += 1;
    }
    fn newline(&mut self) {
        self.lines.push(self.current.iter().collect());
        self.current.clear();
        self.cursor = 0;
        if self.max_lines == 0 {
            self.lines.clear();
        } else if self.lines.len() + 1 > self.max_lines {
            self.lines.remove(0);
        }
    }
}
