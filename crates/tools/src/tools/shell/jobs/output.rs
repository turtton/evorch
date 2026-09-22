//! Complete-line redaction plus bounded live history and final artifact capture.
use crate::output::Capture;
use secret_guard::SecretRedactor;
const LIVE_BYTES: usize = 64 * 1024;

pub(super) struct LiveOutput {
    pub(super) capture: Capture,
    pub(super) text: String,
    pub(super) offset: u64,
    pending: [Vec<u8>; 2],
    discarding: [bool; 2],
    private_key: [bool; 2],
    redactor: SecretRedactor,
}
impl Default for LiveOutput {
    fn default() -> Self {
        Self {
            capture: Capture::default(),
            text: String::new(),
            offset: 0,
            pending: [Vec::new(), Vec::new()],
            discarding: [false; 2],
            private_key: [false; 2],
            redactor: SecretRedactor::from_env(),
        }
    }
}
impl LiveOutput {
    pub(super) fn push(&mut self, stream: usize, bytes: &[u8]) {
        self.capture.push(bytes);
        // Do not publish partial secret-shaped strings across read boundaries.
        // Live previews flush complete lines; EOF also flushes an unfinished line.
        for byte in bytes {
            if !self.discarding[stream] {
                self.pending[stream].push(*byte);
                if self.pending[stream].len() >= LIVE_BYTES {
                    let partial = self
                        .redactor
                        .redact(&String::from_utf8_lossy(&self.pending[stream]))
                        .text;
                    self.private_key[stream] |= partial.contains("[REDACTED:private-key-block]");
                    self.pending[stream].clear();
                    self.discarding[stream] = true;
                    self.append("[oversized output line omitted from live preview]\n");
                }
            }
            if *byte == b'\n' {
                self.flush(stream);
                self.discarding[stream] = false;
            }
        }
    }
    pub(super) fn flush(&mut self, stream: usize) {
        let line = std::mem::take(&mut self.pending[stream]);
        if !line.is_empty() {
            let raw = String::from_utf8_lossy(&line);
            let end_key = raw.contains("-----END ") && raw.contains("PRIVATE KEY-----");
            if self.private_key[stream] {
                if end_key {
                    self.private_key[stream] = false;
                }
                return;
            }
            let text = self.redactor.redact(&raw).text;
            self.private_key[stream] = text.contains("[REDACTED:private-key-block]") && !end_key;
            self.append(&text);
        }
    }
    fn append(&mut self, text: &str) {
        self.text.push_str(text);
        let mut remove = if self.text.len() > LIVE_BYTES {
            self.text
                .len()
                .saturating_sub(LIVE_BYTES)
                .max(LIVE_BYTES / 2)
        } else {
            0
        };
        while !self.text.is_char_boundary(remove) {
            remove += 1;
        }
        if remove > 0 {
            self.text.drain(..remove);
            self.offset += remove as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::LiveOutput;
    #[test]
    fn live_cursor_marks_dropped_output_and_remains_utf8_aligned() {
        let mut output = LiveOutput::default();
        for _ in 0..3000 {
            output.push(0, "日本語のテスト行です\n".as_bytes());
        }
        assert!(output.offset > 0);
        assert!(output.text.len() <= 64 * 1024);
        assert!(output.text.is_char_boundary(0));
    }
}
