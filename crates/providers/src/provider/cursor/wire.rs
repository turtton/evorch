//! Small lossless protobuf envelope codec for Cursor's evolving Connect protocol.
//!
//! Field numbers follow oh-my-pi 602b6c8 `agent.proto` / `cursor-models.proto`
//! (OMP-LICENSE). Opaque checkpoints retain unknown fields byte-for-byte.
use crate::ProviderError;
use serde_json::Value;

pub(super) const MAX_FRAME: usize = 16 * 1024 * 1024;
#[derive(Clone, Default, Debug)]
pub(super) struct Proto(pub Vec<u8>);
#[derive(Clone, Copy)]
pub(super) struct Field<'a> {
    pub number: u32,
    pub data: &'a [u8],
    pub integer: Option<u64>,
    pub wire: u8,
    pub raw: &'a [u8],
}
fn invalid() -> ProviderError {
    ProviderError::InvalidSse {
        detail: "Malformed Cursor protobuf message".into(),
    }
}
fn varint(mut value: u64, output: &mut Vec<u8>) {
    while value > 0x7f {
        output.push(value as u8 | 0x80);
        value >>= 7;
    }
    output.push(value as u8);
}
fn read_varint(data: &[u8], offset: &mut usize) -> Result<u64, ProviderError> {
    let mut value = 0;
    for shift in (0..70).step_by(7) {
        let byte = *data.get(*offset).ok_or_else(invalid)?;
        *offset += 1;
        if shift == 63 && byte > 1 {
            return Err(invalid());
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(invalid())
}
impl Proto {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn bytes(mut self, number: u32, bytes: impl AsRef<[u8]>) -> Self {
        let bytes = bytes.as_ref();
        varint(u64::from(number) << 3 | 2, &mut self.0);
        varint(bytes.len() as u64, &mut self.0);
        self.0.extend_from_slice(bytes);
        self
    }
    pub fn string(self, number: u32, value: &str) -> Self {
        self.bytes(number, value.as_bytes())
    }
    pub fn message(self, number: u32, value: Proto) -> Self {
        self.bytes(number, value.0)
    }
    pub fn integer(mut self, number: u32, value: u64) -> Self {
        varint(u64::from(number) << 3, &mut self.0);
        varint(value, &mut self.0);
        self
    }
    pub fn fields(&self) -> Result<Vec<Field<'_>>, ProviderError> {
        fields(&self.0)
    }
    pub fn without(&self, omitted: &[u32]) -> Result<Self, ProviderError> {
        let mut output = Vec::new();
        for field in self.fields()? {
            if !omitted.contains(&field.number) {
                output.extend_from_slice(field.raw);
            }
        }
        Ok(Self(output))
    }
}
pub(super) fn fields(bytes: &[u8]) -> Result<Vec<Field<'_>>, ProviderError> {
    if bytes.len() > MAX_FRAME {
        return Err(invalid());
    }
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        if output.len() >= 65_536 {
            return Err(invalid());
        }
        let start = offset;
        let tag = read_varint(bytes, &mut offset)?;
        let number = u32::try_from(tag >> 3).map_err(|_| invalid())?;
        if number == 0 || number > 0x1fff_ffff {
            return Err(invalid());
        }
        let wire = (tag & 7) as u8;
        let (length, integer) = match wire {
            0 => (0, Some(read_varint(bytes, &mut offset)?)),
            1 => (8, None),
            2 => (
                usize::try_from(read_varint(bytes, &mut offset)?).map_err(|_| invalid())?,
                None,
            ),
            5 => (4, None),
            _ => return Err(invalid()),
        };
        let end = offset
            .checked_add(length)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(invalid)?;
        output.push(Field {
            number,
            wire,
            integer,
            data: &bytes[offset..end],
            raw: &bytes[start..end],
        });
        offset = end;
    }
    Ok(output)
}
pub(super) fn nested(bytes: &[u8], field: u32) -> Result<Option<&[u8]>, ProviderError> {
    Ok(fields(bytes)?
        .into_iter()
        .rev()
        .find(|f| f.number == field && f.wire == 2)
        .map(|f| f.data))
}
pub(super) fn string(bytes: &[u8], field: u32) -> Result<String, ProviderError> {
    String::from_utf8(nested(bytes, field)?.unwrap_or_default().to_vec()).map_err(|_| invalid())
}
pub(super) fn integer(bytes: &[u8], field: u32) -> Result<Option<u64>, ProviderError> {
    Ok(fields(bytes)?
        .into_iter()
        .rev()
        .find(|f| f.number == field)
        .and_then(|f| f.integer))
}
pub(super) fn frame(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len() + 5);
    output.push(0);
    output.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    output.extend_from_slice(bytes);
    output
}
/// google.protobuf.Value, used for both MCP JSON schemas and argument values.
pub(super) fn encode_value(value: &Value) -> Proto {
    match value {
        Value::Null => Proto::new().integer(1, 0),
        Value::Bool(value) => Proto::new().integer(4, u64::from(*value)),
        Value::String(value) => Proto::new().string(3, value),
        Value::Number(value) => {
            let mut output = vec![17];
            output.extend_from_slice(&value.as_f64().unwrap_or_default().to_le_bytes());
            Proto(output)
        }
        Value::Array(values) => Proto::new().message(
            6,
            values
                .iter()
                .fold(Proto::new(), |p, v| p.message(1, encode_value(v))),
        ),
        Value::Object(values) => Proto::new().message(
            5,
            values.iter().fold(Proto::new(), |p, (k, v)| {
                p.message(1, Proto::new().string(1, k).message(2, encode_value(v)))
            }),
        ),
    }
}
pub(super) fn decode_value(bytes: &[u8]) -> Result<Value, ProviderError> {
    decode_value_at(bytes, 0)
}
fn decode_value_at(bytes: &[u8], depth: usize) -> Result<Value, ProviderError> {
    if depth > 64 {
        return Err(invalid());
    }
    let field = fields(bytes)?.into_iter().last().ok_or_else(invalid)?;
    match (field.number, field.wire) {
        (1, 0) => Ok(Value::Null),
        (2, 1) => {
            let number = f64::from_le_bytes(field.data.try_into().map_err(|_| invalid())?);
            // protobuf.Value stores every JSON number as double. Recover integral
            // JSON numbers for canonical tools that deserialize integer arguments.
            if number.fract() == 0.0 && number >= 0.0 && number < u64::MAX as f64 {
                return Ok(Value::from(number as u64));
            }
            if number.fract() == 0.0 && number >= i64::MIN as f64 && number < 0.0 {
                return Ok(Value::from(number as i64));
            }
            serde_json::Number::from_f64(number)
                .map(Value::Number)
                .ok_or_else(invalid)
        }
        (3, 2) => Ok(Value::String(
            String::from_utf8(field.data.to_vec()).map_err(|_| invalid())?,
        )),
        (4, 0) => Ok(Value::Bool(field.integer == Some(1))),
        (5, 2) => {
            let mut values = serde_json::Map::new();
            for entry in fields(field.data)?
                .into_iter()
                .filter(|f| f.number == 1 && f.wire == 2)
            {
                values.insert(
                    string(entry.data, 1)?,
                    decode_value_at(nested(entry.data, 2)?.ok_or_else(invalid)?, depth + 1)?,
                );
            }
            Ok(Value::Object(values))
        }
        (6, 2) => Ok(Value::Array(
            fields(field.data)?
                .into_iter()
                .filter(|f| f.number == 1 && f.wire == 2)
                .map(|f| decode_value_at(f.data, depth + 1))
                .collect::<Result<_, _>>()?,
        )),
        _ => Err(invalid()),
    }
}
