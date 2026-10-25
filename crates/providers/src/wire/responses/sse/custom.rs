use crate::error::ProviderError;
use crate::stream::StreamEvent;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Phase {
    #[default]
    Unopened,
    Open,
    Closed,
}

#[derive(Debug, Clone, Default)]
pub(super) struct CustomToolState {
    pub(super) index: usize,
    call_id: Option<String>,
    name: Option<String>,
    raw: String,
    phase: Phase,
}

impl CustomToolState {
    pub(super) fn new(index: usize) -> Self {
        Self {
            index,
            ..Self::default()
        }
    }
    pub(super) fn is_open(&self) -> bool {
        self.phase == Phase::Open
    }

    pub(super) fn item(
        &mut self,
        call_id: String,
        name: String,
        input: &str,
        done: bool,
    ) -> Result<StreamEvent, ProviderError> {
        self.call_id = Some(call_id);
        self.name = Some(name);
        let arguments_delta = if done {
            self.close(input)?
        } else if self.phase == Phase::Unopened {
            self.append(input)?
        } else if input.is_empty() || self.raw.starts_with(input) {
            // added が delta より遅れて届いても入力を重複して連結しない。
            String::new()
        } else {
            return Err(self.invalid("added の入力が受信済み差分と一致しません"));
        };
        Ok(self.event(self.call_id.clone(), self.name.clone(), arguments_delta))
    }

    pub(super) fn delta(&mut self, input: &str) -> Result<StreamEvent, ProviderError> {
        let arguments_delta = self.append(input)?;
        Ok(self.event(None, None, arguments_delta))
    }

    pub(super) fn done(&mut self, input: &str) -> Result<Option<StreamEvent>, ProviderError> {
        let arguments_delta = self.close(input)?;
        Ok((!arguments_delta.is_empty()).then(|| self.event(None, None, arguments_delta)))
    }

    fn append(&mut self, input: &str) -> Result<String, ProviderError> {
        let prefix = match self.phase {
            Phase::Unopened => "{\"input\":\"",
            Phase::Open => "",
            Phase::Closed => return Err(self.invalid("close 後に delta を受信しました")),
        };
        self.raw.push_str(input);
        self.phase = Phase::Open;
        Ok(format!("{prefix}{}", escape(input)))
    }

    fn close(&mut self, input: &str) -> Result<String, ProviderError> {
        if self.phase == Phase::Closed {
            return if input == self.raw {
                Ok(String::new())
            } else {
                Err(self.invalid("close 後に異なる確定値を受信しました"))
            };
        }
        let suffix = input
            .strip_prefix(&self.raw)
            .ok_or_else(|| self.invalid("確定値が受信済み差分と一致しません"))?;
        let prefix = if self.phase == Phase::Unopened {
            "{\"input\":\""
        } else {
            ""
        };
        let arguments_delta = format!("{prefix}{}\"}}", escape(suffix));
        self.raw = input.to_owned();
        self.phase = Phase::Closed;
        Ok(arguments_delta)
    }

    fn event(
        &self,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    ) -> StreamEvent {
        StreamEvent::ToolCallDelta {
            index: self.index,
            id,
            name,
            arguments_delta,
        }
    }

    fn invalid(&self, detail: &str) -> ProviderError {
        ProviderError::InvalidSse {
            detail: format!(
                "Responses custom tool input: {detail} (output_index={})",
                self.index
            ),
        }
    }
}

// JSON 文字列の内側だけを送り、どの UTF-8 境界で分割しても同じ JSON に戻す。
fn escape(input: &str) -> String {
    let quoted = serde_json::to_string(input).expect("string serializes as JSON");
    quoted[1..quoted.len() - 1].to_owned()
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn escaping_concatenates_at_every_character_boundary() {
        let input = "\"\\\n\t\r\u{0000}\u{001f}日本語";
        for split in input.char_indices().map(|(i, _)| i).chain([input.len()]) {
            let (a, b) = input.split_at(split);
            assert_eq!(escape(a) + &escape(b), escape(input));
        }
    }
}
