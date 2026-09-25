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

    pub fn inconsistent(expected: &str, got: &str) -> Self {
        Self::new(
            932,
            format!("inconsistent datatypes: expected {expected} got {got}"),
        )
    }

    pub fn table_not_found() -> Self {
        Self::new(942, "table or view does not exist")
    }

    pub fn invalid_identifier(name: &str) -> Self {
        Self::new(904, format!("{name}: invalid identifier"))
    }

    pub fn missing_expression() -> Self {
        Self::new(936, "missing expression")
    }
}
