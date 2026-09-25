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
mod plsql;
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
    /// One entry per bind position, set for PL/SQL blocks and DML with RETURNING.
    /// Empty for other statements.
    pub out_binds: Vec<OutBind>,
}

/// How a statement uses a bind position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BindDir {
    /// A value read by the statement.
    In,
    /// A PL/SQL block's bind: read, and possibly assigned.
    InOut,
    /// A target of `RETURNING ... INTO`: no value is sent, one comes back per row.
    Returning,
}

/// The value of a bind after a statement ran.
#[derive(Debug, Clone, PartialEq)]
pub enum OutBind {
    /// An input-only bind: nothing comes back.
    In,
    /// The final value of a PL/SQL block's bind.
    Value(Value),
    /// The values a RETURNING clause produced, one per affected row.
    Returning(Vec<Value>),
}

/// How each bind position of `sql` is used. Fails if the statement does not parse.
pub fn bind_directions(sql: &str) -> Result<Vec<BindDir>, OraError> {
    Ok(parser::parse(sql)?.bind_dirs)
}

/// A saved copy of a database's committed tables, for [`Database::restore`].
#[derive(Debug, Clone)]
pub struct Snapshot {
    catalog: Catalog,
    identities: HashMap<String, i64>,
    sequences: HashMap<String, i128>,
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
    /// The next value NEXTVAL returns for each sequence. Like Oracle, sequences are not
    /// transactional: a rollback does not give numbers back.
    sequences: Mutex<HashMap<String, i128>>,
}

impl Default for Database {
    fn default() -> Self {
        Self {
            state: RwLock::new(Catalog::default()),
            next_row_id: AtomicU64::new(1),
            next_constraint: AtomicU64::new(10_000),
            identities: Mutex::new(HashMap::new()),
            sequences: Mutex::new(HashMap::new()),
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
            sequences: self.sequences.lock().unwrap().clone(),
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
        *self.sequences.lock().unwrap() = snapshot.sequences.clone();
    }

    /// Drops every table, sequence, procedure and function.
    pub fn reset(&self) {
        self.restore(&Snapshot {
            catalog: Catalog::default(),
            identities: HashMap::new(),
            sequences: HashMap::new(),
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

    /// A stored procedure or function. Like all DDL, routines are visible to every
    /// session at once, including sessions in the middle of a transaction.
    fn routine(&self, name: &str) -> Option<Arc<plsql::Routine>> {
        self.state.read().unwrap().routines.get(name).cloned()
    }

    fn sequence_def(&self, name: &str) -> Option<catalog::SequenceDef> {
        self.state.read().unwrap().sequences.get(name).cloned()
    }

    /// The next value of a sequence.
    fn next_sequence(&self, name: &str, def: &catalog::SequenceDef) -> Result<i128, OraError> {
        let mut seqs = self.sequences.lock().unwrap();
        let next = seqs.entry(name.to_string()).or_insert(def.start);
        let mut v = *next;
        if v > def.max || v < def.min {
            if !def.cycle {
                let limit = if def.increment > 0 {
                    "MAXVALUE"
                } else {
                    "MINVALUE"
                };
                return Err(OraError::new(
                    8004,
                    format!("sequence {name}.NEXTVAL exceeds {limit} and cannot be instantiated"),
                ));
            }
            v = if def.increment > 0 { def.min } else { def.max };
        }
        *next = v.saturating_add(def.increment);
        Ok(v)
    }

    /// Sets where a sequence starts over, or forgets it so it starts at its START WITH.
    fn set_sequence(&self, name: &str, next: Option<i128>) {
        let mut seqs = self.sequences.lock().unwrap();
        match next {
            Some(n) => {
                seqs.insert(name.to_string(), n);
            }
            None => {
                seqs.remove(name);
            }
        }
    }

    /// The value the next NEXTVAL would return, if the sequence has been used.
    fn peek_sequence(&self, name: &str) -> Option<i128> {
        self.sequences.lock().unwrap().get(name).copied()
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

/// Whether a statement starts a PL/SQL unit: a block, or CREATE of a procedure, function,
/// package, trigger or type.
fn is_plsql_unit(stmt: &str) -> bool {
    let words: Vec<String> = stmt
        .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '<'))
        .filter(|w| !w.is_empty())
        .take(5)
        .map(str::to_uppercase)
        .collect();
    let mut w = words.iter().map(String::as_str);
    match w.next() {
        Some("BEGIN" | "DECLARE") => true,
        Some(first) if first.starts_with("<<") => true,
        Some("CREATE") => {
            let mut next = w.next();
            if next == Some("OR") {
                w.next();
                next = w.next();
            }
            if matches!(next, Some("EDITIONABLE" | "NONEDITIONABLE")) {
                next = w.next();
            }
            matches!(
                next,
                Some("PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE")
            )
        }
        _ => false,
    }
}

/// Splits a SQL script into statements on `;` or on a line holding only `/`,
/// ignoring separators inside strings, quoted identifiers and comments. PL/SQL blocks,
/// procedures and functions run to the next `/` line (or the end of the script).
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
            // PL/SQL units hold semicolons of their own and end at a `/` line.
            ';' if is_plsql_unit(&current) => current.push(c),
            ';' => flush(&mut current, &mut out),
            // A line holding only `/` (and whitespace) ends a statement.
            '/' if at_line_start
                && chars
                    .clone()
                    .take_while(|n| *n != '\n')
                    .all(char::is_whitespace) =>
            {
                while chars.peek().is_some_and(|n| *n != '\n') {
                    chars.next();
                }
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

#[cfg(test)]
mod plsql_tests;
