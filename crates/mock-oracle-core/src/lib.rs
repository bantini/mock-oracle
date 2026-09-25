//! The mock-oracle engine: parsing and executing Oracle SQL and PL/SQL against
//! in-memory storage. It does no I/O; the server and language bindings are thin
//! layers over [`Database`].

mod ast;
mod catalog;
mod datetime;
mod error;
mod eval;
mod functions;
mod lexer;
mod parser;
mod session;
mod value;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

pub use error::OraError;
pub use session::Session;
pub use value::{format_number, SqlType, Value};

use catalog::Catalog;

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

/// A saved copy of a database's committed tables, for [`Database::restore`].
#[derive(Debug, Clone)]
pub struct Snapshot {
    catalog: Catalog,
    identities: HashMap<String, i64>,
}

/// One in-memory database. Sessions share its committed state; each session sees
/// its own uncommitted changes until it commits.
#[derive(Debug)]
pub struct Database {
    state: RwLock<Catalog>,
    next_row_id: AtomicU64,
    next_constraint: AtomicU64,
    /// Next value of each table's identity column.
    identities: Mutex<HashMap<String, i64>>,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            state: RwLock::new(Catalog::default()),
            next_row_id: AtomicU64::new(1),
            next_constraint: AtomicU64::new(10_000),
            identities: Mutex::new(HashMap::new()),
        }
    }
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a session. `user` is the schema name shown in error messages and by USER.
    pub fn session(self: &Arc<Self>, user: &str) -> Session {
        Session::new(self.clone(), user)
    }

    /// Executes one statement in its own auto-committing session.
    pub fn execute(self: &Arc<Self>, sql: &str, binds: &[Value]) -> Result<QueryResult, OraError> {
        let mut s = self.session("MOCK");
        let r = s.execute(sql, binds)?;
        s.commit()?;
        Ok(r)
    }

    /// Runs a script of statements separated by `;` (or `/` on its own line), committing
    /// at the end. Stops at the first error.
    pub fn run_script(self: &Arc<Self>, script: &str) -> Result<(), OraError> {
        let mut s = self.session("MOCK");
        for stmt in split_script(script) {
            s.execute(&stmt, &[]).map_err(|e| {
                OraError::new(e.code, format!("{} (in: {})", e.message, abbreviate(&stmt)))
            })?;
        }
        s.commit()
    }

    /// Captures the committed state of every table.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            catalog: self.committed(),
            identities: self.identities.lock().unwrap().clone(),
        }
    }

    /// Replaces the committed state with a snapshot. Open transactions in other sessions
    /// are replayed onto it if they commit later.
    pub fn restore(&self, snapshot: &Snapshot) {
        let mut state = self.state.write().unwrap();
        let version = state.version + 1;
        *state = snapshot.catalog.clone();
        state.version = version;
        *self.identities.lock().unwrap() = snapshot.identities.clone();
    }

    /// Drops every table.
    pub fn reset(&self) {
        self.restore(&Snapshot {
            catalog: Catalog::default(),
            identities: HashMap::new(),
        });
    }

    fn committed(&self) -> Catalog {
        self.state.read().unwrap().clone()
    }

    fn next_row_id(&self) -> u64 {
        self.next_row_id.fetch_add(1, Ordering::Relaxed)
    }

    fn next_identity(&self, table: &str) -> i64 {
        let mut ids = self.identities.lock().unwrap();
        let next = ids.entry(table.to_string()).or_insert(1);
        let v = *next;
        *next += 1;
        v
    }

    /// A system-generated constraint name such as `SYS_C0010001`.
    fn constraint_name(&self) -> String {
        format!(
            "SYS_C{:07}",
            self.next_constraint.fetch_add(1, Ordering::Relaxed)
        )
    }
}

fn abbreviate(sql: &str) -> String {
    let one_line: String = sql.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 60 {
        one_line.chars().take(57).collect::<String>() + "..."
    } else {
        one_line
    }
}

/// Splits a SQL script into statements on `;` or on a line holding only `/`,
/// ignoring separators inside strings, quoted identifiers and comments.
pub fn split_script(script: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = script.chars().peekable();
    let mut at_line_start = true;
    let flush = |current: &mut String, out: &mut Vec<String>| {
        let stmt = current.trim();
        if !stmt.is_empty() {
            out.push(stmt.to_string());
        }
        current.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                current.push(c);
                for d in chars.by_ref() {
                    current.push(d);
                    if d == c {
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                for d in chars.by_ref() {
                    if d == '\n' {
                        current.push('\n');
                        break;
                    }
                }
                at_line_start = true;
                continue;
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut prev = ' ';
                for d in chars.by_ref() {
                    if prev == '*' && d == '/' {
                        break;
                    }
                    prev = d;
                }
                current.push(' ');
            }
            ';' => flush(&mut current, &mut out),
            '/' if at_line_start && chars.peek().map_or(true, |n| *n == '\n' || *n == '\r') => {
                flush(&mut current, &mut out)
            }
            _ => current.push(c),
        }
        if c == '\n' {
            at_line_start = true;
        } else if !c.is_whitespace() {
            at_line_start = false;
        }
    }
    flush(&mut current, &mut out);
    out
}

#[cfg(test)]
mod tests;
