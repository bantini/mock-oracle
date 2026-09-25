use thiserror::Error;

/// An Oracle error, rendered the way Oracle does: `ORA-00942: table or view does not exist`.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("ORA-{code:05}: {message}")]
pub struct OraError {
    pub code: u32,
    pub message: String,
}

impl OraError {
    pub fn new(code: u32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}
