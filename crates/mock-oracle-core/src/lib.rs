//! The mock-oracle engine: parsing and executing Oracle SQL and PL/SQL against
//! in-memory storage. It does no I/O; the server and language bindings are thin
//! layers over [`Database`].

mod ast;
mod error;
mod eval;
mod lexer;
mod parser;
mod value;

pub use error::OraError;
pub use value::{format_number, SqlType, Value};

/// A result column.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    pub sql_type: SqlType,
}

/// The result of executing one statement.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct QueryResult {
    /// True for statements that return rows (queries), even when no rows match.
    pub is_query: bool,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// Rows inserted, updated or deleted by a DML statement.
    pub rows_affected: u64,
}

/// One in-memory database instance. Sessions share its committed state.
#[derive(Debug, Default)]
pub struct Database {}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Executes one SQL statement. `binds` are the bind values in the order the
    /// placeholders appear in the statement.
    pub fn execute(&self, sql: &str, binds: &[Value]) -> Result<QueryResult, OraError> {
        let stmt = parser::parse(sql)?;
        let expected = parser::count_binds(&stmt);
        if binds.len() < expected {
            return Err(OraError::new(1008, "not all variables bound"));
        }
        eval::execute(&stmt, binds)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(sql: &str, binds: &[Value]) -> QueryResult {
        Database::new().execute(sql, binds).unwrap()
    }

    fn err(sql: &str) -> String {
        Database::new().execute(sql, &[]).unwrap_err().to_string()
    }

    #[test]
    fn select_one_from_dual() {
        let r = query("SELECT 1 FROM DUAL", &[]);
        assert!(r.is_query);
        assert_eq!(
            r.columns,
            vec![Column {
                name: "1".into(),
                sql_type: SqlType::Number
            }]
        );
        assert_eq!(r.rows, vec![vec![Value::parse_number("1").unwrap()]]);
    }

    #[test]
    fn expressions_strings_and_nulls() {
        let r = query(
            "select 2 * (3 + 4) n, 'a' || 1 || null s, '' e, dummy from dual",
            &[],
        );
        assert_eq!(
            r.rows[0],
            vec![
                Value::parse_number("14").unwrap(),
                Value::varchar("a1"),
                Value::Null,
                Value::varchar("X")
            ]
        );
        assert_eq!(r.columns[1].sql_type, SqlType::Varchar2(2));
        assert_eq!(r.columns[2].sql_type, SqlType::Varchar2(0));
    }

    #[test]
    fn star_from_dual() {
        let r = query("select * from dual", &[]);
        assert_eq!(
            r.columns,
            vec![Column {
                name: "DUMMY".into(),
                sql_type: SqlType::Varchar2(1)
            }]
        );
        assert_eq!(r.rows, vec![vec![Value::varchar("X")]]);
    }

    #[test]
    fn binds_and_implicit_conversion() {
        let r = query(
            "select :a + 1, :b from dual",
            &[Value::varchar("41"), Value::varchar("hi")],
        );
        assert_eq!(
            r.rows[0],
            vec![Value::parse_number("42").unwrap(), Value::varchar("hi")]
        );
    }

    #[test]
    fn oracle_errors() {
        assert_eq!(
            err("select 1/0 from dual"),
            "ORA-01476: divisor is equal to zero"
        );
        assert_eq!(err("select 'x' + 1 from dual"), "ORA-01722: invalid number");
        assert_eq!(
            err("select foo from dual"),
            "ORA-00904: \"FOO\": invalid identifier"
        );
        assert_eq!(
            err("select :a from dual"),
            "ORA-01008: not all variables bound"
        );
        assert_eq!(err("FROB"), "ORA-00900: invalid SQL statement");
    }
}
