//! Bound checks shared by every wire type.
//!
//! Deserialization only checks shape. Anything that arrives from a peer is also
//! passed through the `validate` method of its type before it is acted upon.

use crate::errors::{ErrorCode, RetryAdvice, WorkbenchError};

/// Why a value was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ValidationError {
    #[error("{field} must not be empty")]
    Empty { field: &'static str },
    #[error("{field} is {actual} bytes; the maximum is {max}")]
    TooLong {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("{field} has {actual} items; the maximum is {max}")]
    TooMany {
        field: &'static str,
        max: usize,
        actual: usize,
    },
    #[error("{field} contains a character that is not allowed")]
    InvalidChars { field: &'static str },
    #[error("{field} is out of range: {reason}")]
    OutOfRange {
        field: &'static str,
        reason: &'static str,
    },
    #[error("{field} is inconsistent: {reason}")]
    Inconsistent {
        field: &'static str,
        reason: &'static str,
    },
}

impl ValidationError {
    /// The offending field.
    pub fn field(&self) -> &'static str {
        match self {
            Self::Empty { field }
            | Self::TooLong { field, .. }
            | Self::TooMany { field, .. }
            | Self::InvalidChars { field }
            | Self::OutOfRange { field, .. }
            | Self::Inconsistent { field, .. } => field,
        }
    }
}

impl From<ValidationError> for WorkbenchError {
    fn from(error: ValidationError) -> Self {
        let code = match error {
            ValidationError::TooLong { .. } | ValidationError::TooMany { .. } => {
                ErrorCode::LimitExceeded
            }
            _ => ErrorCode::InvalidRequest,
        };
        let mut out = WorkbenchError::new(code, error.to_string(), RetryAdvice::Never);
        out.details = Some(crate::errors::ErrorDetails::Field {
            field: error.field().to_owned(),
        });
        out
    }
}

/// Single-line text: no control characters at all.
pub fn check_line(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), ValidationError> {
    check_bytes(field, value, max_bytes, allow_empty)?;
    if value.chars().any(char::is_control) {
        return Err(ValidationError::InvalidChars { field });
    }
    Ok(())
}

/// Multi-line text: `\t`, `\n` and `\r` are the only control characters.
///
/// This bounds JSON escaping growth at 2x, which the frame budget relies on.
pub fn check_text(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), ValidationError> {
    check_bytes(field, value, max_bytes, allow_empty)?;
    if value
        .chars()
        .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
    {
        return Err(ValidationError::InvalidChars { field });
    }
    Ok(())
}

/// Length-only check, for content the core relays unchanged (message bodies).
pub fn check_bytes(
    field: &'static str,
    value: &str,
    max_bytes: usize,
    allow_empty: bool,
) -> Result<(), ValidationError> {
    if value.is_empty() && !allow_empty {
        return Err(ValidationError::Empty { field });
    }
    if value.len() > max_bytes {
        return Err(ValidationError::TooLong {
            field,
            max: max_bytes,
            actual: value.len(),
        });
    }
    Ok(())
}

/// Collection size bound.
pub fn check_count(field: &'static str, actual: usize, max: usize) -> Result<(), ValidationError> {
    if actual > max {
        return Err(ValidationError::TooMany { field, max, actual });
    }
    Ok(())
}

/// Validate every item of a slice, stopping at the first failure.
pub fn check_all<T>(
    items: &[T],
    check: impl Fn(&T) -> Result<(), ValidationError>,
) -> Result<(), ValidationError> {
    items.iter().try_for_each(check)
}
