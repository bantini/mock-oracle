//! The mock-oracle engine: parsing, planning and executing Oracle SQL and PL/SQL
//! against in-memory storage. It does no I/O; the server and language bindings
//! are thin layers over [`Database`].

mod error;

pub use error::OraError;

/// A value as Oracle sees it. The empty string is never stored: Oracle treats `''` as NULL.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Number(String),
    Varchar2(String),
}

/// The result of executing one statement.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub rows_affected: u64,
}

/// One in-memory database instance. Sessions share its committed state.
#[derive(Debug, Default)]
pub struct Database {}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes one SQL statement or PL/SQL block.
    pub fn execute(&self, sql: &str) -> Result<QueryResult, OraError> {
        let _ = sql;
        Err(OraError::new(900, "invalid SQL statement"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_statement_reports_ora_00900() {
        let err = Database::new().execute("FROB").unwrap_err();
        assert_eq!(err.to_string(), "ORA-00900: invalid SQL statement");
    }
}
