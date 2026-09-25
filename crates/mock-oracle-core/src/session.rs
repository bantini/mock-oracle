//! Sessions: statement execution, transactions, DML and DDL.

use std::collections::HashSet;
use std::sync::Arc;

use bigdecimal::RoundingMode;

use crate::ast::*;
use crate::catalog::SequenceDef;
use crate::catalog::{Catalog, ConstraintKind, Table, TableColumn};
use crate::eval::{infer_type, Env, Ex, RelCol, Scope, VarLookup};
use crate::plsql::exec::{self, Host, Outcome};
use crate::plsql::Routine;
use crate::{datetime, parser, Column, Database, OraError, OutBind, QueryResult, SqlType, Value};

/// One change in a transaction, replayed onto the committed state at COMMIT.
#[derive(Debug, Clone)]
enum Change {
    Insert {
        table: String,
        id: u64,
        values: Vec<Value>,
    },
    Update {
        table: String,
        id: u64,
        values: Vec<Value>,
        /// The values before the change, to undo a failed statement.
        old: Vec<Value>,
    },
    Delete {
        table: String,
        id: u64,
        old: Vec<Value>,
    },
}

#[derive(Debug)]
struct Txn {
    /// The committed version this transaction started from.
    base_version: u64,
    /// The committed state plus this transaction's changes.
    work: Catalog,
    log: Vec<Change>,
}

/// A point in a session's transaction that later changes can be undone to.
#[derive(Debug, Clone, Copy)]
pub struct Savepoint {
    commits: u64,
    changes: usize,
}

/// A connection's view of a [`Database`]. Changes are private to the session until
/// [`Session::commit`]; dropping the session rolls them back.
#[derive(Debug)]
pub struct Session {
    db: Arc<Database>,
    env: Env,
    txn: Option<Txn>,
    /// Counts commits and rollbacks, so a failed block knows whether it may undo
    /// changes made before it.
    commits: u64,
}

fn unique_violation(user: &str, constraint: &str) -> OraError {
    OraError::new(
        1,
        format!("unique constraint ({user}.{constraint}) violated"),
    )
}

impl Session {
    pub fn new(db: Arc<Database>, user: &str) -> Self {
        Self {
            db,
            env: Env::new(user),
            txn: None,
            commits: 0,
        }
    }

    pub fn user(&self) -> &str {
        &self.env.user
    }

    /// Whether the session has uncommitted changes.
    pub fn in_transaction(&self) -> bool {
        self.txn.as_ref().is_some_and(|t| !t.log.is_empty())
    }

    /// The session time zone, in minutes east of UTC.
    pub fn time_zone_offset(&self) -> i32 {
        self.env.tz_offset_minutes
    }

    /// Executes one SQL statement or PL/SQL block. `binds` are the bind values by
    /// position (see [`crate::bind_directions`]); values at RETURNING positions are
    /// ignored.
    pub fn execute(&mut self, sql: &str, binds: &[Value]) -> Result<QueryResult, OraError> {
        let parsed = parser::parse(sql)?;
        if binds.len() < parsed.binds {
            return Err(OraError::new(1008, "not all variables bound"));
        }
        self.env.start_statement();
        match &parsed.stmt {
            Statement::Block(block) => {
                let values = self.block(block, binds[..parsed.binds].to_vec())?;
                Ok(QueryResult {
                    out_binds: values.into_iter().map(OutBind::Value).collect(),
                    ..Default::default()
                })
            }
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                let outcome = self.run(&parsed.stmt, binds, None)?;
                let mut out_binds = Vec::new();
                if let Some(r) = returning(&parsed.stmt) {
                    out_binds = vec![OutBind::In; parsed.binds];
                    for (k, e) in r.into.iter().enumerate() {
                        if let Expr::Bind(i) = e {
                            out_binds[*i] = OutBind::Returning(
                                outcome.rows.iter().map(|row| row[k].clone()).collect(),
                            );
                        }
                    }
                }
                Ok(QueryResult {
                    rows_affected: outcome.rows_affected,
                    out_binds,
                    ..Default::default()
                })
            }
            stmt => {
                let outcome = self.run(stmt, binds, None)?;
                Ok(QueryResult {
                    is_query: matches!(stmt, Statement::Query(_)),
                    columns: outcome.columns,
                    rows: outcome.rows,
                    ..Default::default()
                })
            }
        }
    }

    /// Runs an anonymous block as one statement: if it fails, its uncommitted changes
    /// are undone.
    fn block(
        &mut self,
        block: &crate::plsql::Block,
        binds: Vec<Value>,
    ) -> Result<Vec<Value>, OraError> {
        let savepoint = self.savepoint();
        let result = exec::run_block(self, block, binds);
        if result.is_err() {
            self.rollback_to(savepoint);
        }
        result
    }

    /// The state this session sees: its transaction's, or the committed state.
    fn catalog(&self) -> std::borrow::Cow<'_, Catalog> {
        match &self.txn {
            Some(t) => std::borrow::Cow::Borrowed(&t.work),
            None => std::borrow::Cow::Owned(self.db.committed()),
        }
    }

    /// Runs a statement other than a top-level block.
    fn run(
        &mut self,
        stmt: &Statement,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
    ) -> Result<Outcome, OraError> {
        match stmt {
            Statement::Query(q) => {
                let cat = self.catalog();
                let rel = Ex {
                    cat: &cat,
                    binds,
                    env: &self.env,
                    db: &self.db,
                    vars,
                }
                .query(q, None)?;
                Ok(Outcome {
                    rows_affected: rel.rows.len() as u64,
                    columns: rel
                        .cols
                        .into_iter()
                        .map(|c| Column {
                            name: c.name,
                            sql_type: c.sql_type,
                        })
                        .collect(),
                    rows: rel.rows,
                })
            }
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                let (rows_affected, rows) = self.dml(stmt, binds, vars)?;
                Ok(Outcome {
                    rows_affected,
                    rows,
                    columns: Vec::new(),
                })
            }
            Statement::Block(block) => {
                self.block(block, binds.to_vec())?;
                Ok(Outcome::default())
            }
            Statement::Commit => self.commit().map(|_| Outcome::default()),
            Statement::Rollback => {
                self.rollback();
                Ok(Outcome::default())
            }
            Statement::AlterSession { name, value } => {
                self.alter_session(name, value)?;
                Ok(Outcome::default())
            }
            stmt => {
                // DDL commits any open transaction first, then changes the committed state directly.
                self.commit()?;
                self.ddl(stmt.clone(), binds)?;
                Ok(Outcome::default())
            }
        }
    }

    pub fn commit(&mut self) -> Result<(), OraError> {
        self.commits += 1;
        let Some(txn) = self.txn.take() else {
            return Ok(());
        };
        if txn.log.is_empty() {
            return Ok(());
        }
        let mut state = self.db.state.write().unwrap();
        if state.version == txn.base_version {
            *state = txn.work;
            state.version = txn.base_version + 1;
            return Ok(());
        }
        // Someone else committed meanwhile: replay our changes onto the newer state.
        let mut next = state.clone();
        for change in txn.log {
            match change {
                Change::Insert { table, id, values } => {
                    if let Some(t) = next
                        .table_mut(&table)
                        .filter(|t| t.columns.len() == values.len())
                    {
                        t.insert(id, values)
                            .map_err(|c| unique_violation(&self.env.user, &c))?;
                    }
                }
                Change::Update {
                    table, id, values, ..
                } => {
                    if let Some(t) = next
                        .table_mut(&table)
                        .filter(|t| t.columns.len() == values.len())
                    {
                        t.update(vec![(id, values)])
                            .map_err(|c| unique_violation(&self.env.user, &c))?;
                    }
                }
                Change::Delete { table, id, .. } => {
                    if let Some(t) = next.table_mut(&table) {
                        t.delete(id);
                    }
                }
            }
        }
        next.version = state.version + 1;
        *state = next;
        Ok(())
    }

    /// Marks the current point of the transaction, to undo later work with
    /// [`Session::rollback_to`].
    pub fn savepoint(&self) -> Savepoint {
        Savepoint {
            commits: self.commits,
            changes: self.txn.as_ref().map_or(0, |t| t.log.len()),
        }
    }

    /// Undoes the uncommitted changes made since `savepoint`. Changes committed since
    /// then stay; only those after the last commit are undone.
    pub fn rollback_to(&mut self, savepoint: Savepoint) {
        let keep = if self.commits == savepoint.commits {
            savepoint.changes
        } else {
            0
        };
        if let Some(txn) = &mut self.txn {
            undo(txn, keep.min(txn.log.len()));
            if txn.log.is_empty() {
                self.txn = None;
            }
        }
    }

    pub fn rollback(&mut self) {
        self.commits += 1;
        self.txn = None;
    }

    fn alter_session(&mut self, name: &str, value: &str) -> Result<(), OraError> {
        match name {
            "NLS_DATE_FORMAT" => {
                datetime::validate_format(value)?;
                self.env.nls_date_format = value.to_string();
            }
            "NLS_TIMESTAMP_FORMAT" => {
                datetime::validate_format(value)?;
                self.env.nls_timestamp_format = value.to_string();
            }
            "TIME_ZONE" => {
                self.env.tz_offset_minutes = parse_time_zone(value)?;
            }
            // CURRENT_SCHEMA, NLS_LANGUAGE, NLS_TERRITORY and the like are accepted and ignored.
            _ => {}
        }
        Ok(())
    }

    // ---- DML ----

    /// Runs INSERT, UPDATE or DELETE. Returns the row count and the RETURNING values.
    fn dml(
        &mut self,
        stmt: &Statement,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
    ) -> Result<(u64, Vec<Vec<Value>>), OraError> {
        let db = self.db.clone();
        let txn = self.txn.get_or_insert_with(|| {
            let work = db.committed();
            Txn {
                base_version: work.version,
                work,
                log: Vec::new(),
            }
        });
        // Statement-level atomicity: a failing statement undoes only its own changes.
        let saved = txn.log.len();
        let ctx = Dml {
            db: &db,
            env: &self.env,
            binds,
            vars,
        };
        let result = match stmt {
            Statement::Insert(i) => insert(&ctx, txn, i),
            Statement::Update(u) => update(&ctx, txn, u),
            Statement::Delete(d) => delete(&ctx, txn, d),
            _ => unreachable!(),
        };
        if result.is_err() {
            undo(txn, saved);
        }
        // Without changes there is no transaction, so later queries see other sessions' commits.
        if txn.log.is_empty() {
            self.txn = None;
        }
        result
    }

    // ---- DDL ----

    fn ddl(&mut self, stmt: Statement, binds: &[Value]) -> Result<(), OraError> {
        let user = self.env.user.clone();
        match stmt {
            Statement::CreateTable(ct) => {
                // CREATE TABLE ... AS SELECT reads the committed state before taking the write lock.
                let from_query = match &ct.as_query {
                    Some(q) => {
                        let cat = self.db.committed();
                        Some(
                            Ex {
                                cat: &cat,
                                binds,
                                env: &self.env,
                                db: &self.db,
                                vars: None,
                            }
                            .query(q, None)?,
                        )
                    }
                    None => None,
                };
                let mut state = self.db.state.write().unwrap();
                if state.name_in_use(&ct.name) {
                    return Err(OraError::new(
                        955,
                        "name is already used by an existing object",
                    ));
                }
                let mut columns = Vec::new();
                let mut seen = HashSet::new();
                let mut rows = Vec::new();
                if let Some(rel) = from_query {
                    for (i, c) in rel.cols.iter().enumerate() {
                        let sql_type =
                            match infer_type(Some(c.sql_type), rel.rows.iter().map(|r| &r[i])) {
                                SqlType::Varchar2(0) => SqlType::Varchar2(1),
                                t => t,
                            };
                        columns.push(TableColumn {
                            name: c.name.clone(),
                            sql_type,
                            default: None,
                            not_null: false,
                            identity: false,
                        });
                    }
                    rows = rel.rows;
                } else {
                    for c in ct.columns {
                        columns.push(TableColumn {
                            name: c.name,
                            sql_type: c.sql_type,
                            default: c.default,
                            not_null: c.not_null,
                            identity: c.identity,
                        });
                    }
                }
                for c in &columns {
                    if !seen.insert(c.name.clone()) {
                        return Err(OraError::new(957, "duplicate column name"));
                    }
                }
                let mut table = Table::new(ct.name.clone(), columns);
                for c in ct.constraints {
                    let name = match c.name {
                        Some(n) => n,
                        None => self.db.constraint_name(),
                    };
                    let resolve =
                        |table: &Table, cols: &[String]| -> Result<Vec<usize>, OraError> {
                            cols.iter()
                                .map(|n| {
                                    table.column_index(n).ok_or_else(|| {
                                        OraError::invalid_identifier(&format!("\"{n}\""))
                                    })
                                })
                                .collect()
                        };
                    match c.kind {
                        crate::ast::ConstraintKind::PrimaryKey(cols) => {
                            if table.has_primary_key() {
                                return Err(OraError::new(
                                    2260,
                                    "table can have only one primary key",
                                ));
                            }
                            let idx = resolve(&table, &cols)?;
                            for &i in &idx {
                                table.columns[i].not_null = true;
                            }
                            let _ = table.add_unique(name, idx, true, false);
                        }
                        crate::ast::ConstraintKind::Unique(cols) => {
                            let idx = resolve(&table, &cols)?;
                            let _ = table.add_unique(name, idx, false, false);
                        }
                        crate::ast::ConstraintKind::Check(e) => {
                            table.constraints.push(crate::catalog::Constraint {
                                name,
                                kind: ConstraintKind::Check(e),
                                is_index: false,
                            })
                        }
                        // Foreign keys are accepted but not enforced.
                        crate::ast::ConstraintKind::ForeignKey { columns, .. } => {
                            resolve(&table, &columns)?;
                        }
                    }
                }
                for values in rows {
                    let id = self.db.next_row_id();
                    table
                        .insert(id, values)
                        .map_err(|c| unique_violation(&user, &c))?;
                }
                self.db.identities.lock().unwrap().remove(&ct.name);
                state.tables.insert(ct.name, Arc::new(table));
                state.version += 1;
            }
            Statement::DropTable { name } => {
                let mut state = self.db.state.write().unwrap();
                if state.tables.remove(&name).is_none() {
                    return Err(OraError::table_not_found());
                }
                self.db.identities.lock().unwrap().remove(&name);
                state.version += 1;
            }
            Statement::Truncate { name } => {
                let mut state = self.db.state.write().unwrap();
                state
                    .table_mut(&name)
                    .ok_or_else(OraError::table_not_found)?
                    .truncate();
                state.version += 1;
            }
            Statement::CreateIndex {
                name,
                table,
                columns,
                unique,
            } => {
                let mut state = self.db.state.write().unwrap();
                if state.name_in_use(&name) {
                    return Err(OraError::new(
                        955,
                        "name is already used by an existing object",
                    ));
                }
                let t = state
                    .table_mut(&table)
                    .ok_or_else(OraError::table_not_found)?;
                let idx = columns
                    .iter()
                    .map(|n| {
                        t.column_index(n)
                            .ok_or_else(|| OraError::invalid_identifier(&format!("\"{n}\"")))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                if unique {
                    t.add_unique(name, idx, false, true).map_err(|_| {
                        OraError::new(1452, "cannot CREATE UNIQUE INDEX; duplicate keys found")
                    })?;
                } else {
                    t.constraints.push(crate::catalog::Constraint {
                        name,
                        kind: ConstraintKind::Index,
                        is_index: true,
                    });
                }
                state.version += 1;
            }
            Statement::DropIndex { name } => {
                let mut state = self.db.state.write().unwrap();
                let owner = state
                    .tables
                    .iter()
                    .find(|(_, t)| t.constraints.iter().any(|c| c.is_index && c.name == name))
                    .map(|(n, _)| n.clone())
                    .ok_or_else(|| OraError::new(1418, "specified index does not exist"))?;
                let t = state.table_mut(&owner).unwrap();
                t.constraints.retain(|c| !(c.is_index && c.name == name));
                state.version += 1;
            }
            Statement::CreateSequence { name, options } => {
                let mut state = self.db.state.write().unwrap();
                if state.name_in_use(&name) {
                    return Err(OraError::new(
                        955,
                        "name is already used by an existing object",
                    ));
                }
                let def = sequence_def(None, &options)?;
                self.db.set_sequence(&name, None);
                state.sequences.insert(name, def);
                state.version += 1;
            }
            Statement::AlterSequence { name, options } => {
                let mut state = self.db.state.write().unwrap();
                let Some(old) = state.sequences.get(&name).cloned() else {
                    return Err(OraError::new(2289, "sequence does not exist"));
                };
                let def = sequence_def(Some(&old), &options)?;
                // The next number follows the last one handed out, with the new increment.
                if let Some(next) = self.db.peek_sequence(&name) {
                    self.db
                        .set_sequence(&name, Some(next - old.increment + def.increment));
                }
                for o in &options {
                    if let SequenceOption::Restart(at) = o {
                        let start = at.unwrap_or(if def.increment > 0 { def.min } else { def.max });
                        self.db.set_sequence(&name, Some(start));
                    }
                }
                state.sequences.insert(name, def);
                state.version += 1;
            }
            Statement::DropSequence { name } => {
                let mut state = self.db.state.write().unwrap();
                if state.sequences.remove(&name).is_none() {
                    return Err(OraError::new(2289, "sequence does not exist"));
                }
                self.db.set_sequence(&name, None);
                state.version += 1;
            }
            Statement::CreateRoutine {
                routine,
                or_replace,
            } => {
                let mut state = self.db.state.write().unwrap();
                let replacing = or_replace && state.routines.contains_key(&routine.name);
                if !replacing && state.name_in_use(&routine.name) {
                    return Err(OraError::new(
                        955,
                        "name is already used by an existing object",
                    ));
                }
                state.routines.insert(routine.name.clone(), routine);
                state.version += 1;
            }
            Statement::DropRoutine { name, function } => {
                let mut state = self.db.state.write().unwrap();
                match state.routines.get(&name) {
                    Some(r) if r.is_function() == function => {
                        state.routines.remove(&name);
                        state.version += 1;
                    }
                    _ => return Err(OraError::new(4043, format!("object {name} does not exist"))),
                }
            }
            _ => unreachable!("not DDL"),
        }
        Ok(())
    }
}

impl Host for Session {
    fn env(&self) -> &Env {
        &self.env
    }

    fn with_ex<R>(
        &mut self,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
        f: impl FnOnce(&Ex) -> R,
    ) -> R {
        let cat = self.catalog();
        f(&Ex {
            cat: &cat,
            binds,
            env: &self.env,
            db: &self.db,
            vars,
        })
    }

    fn execute(
        &mut self,
        stmt: &Statement,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
    ) -> Result<Outcome, OraError> {
        self.run(stmt, binds, vars)
    }

    fn routine(&self, name: &str) -> Option<Arc<Routine>> {
        self.db.routine(name)
    }
}

/// The RETURNING clause of a DML statement.
fn returning(stmt: &Statement) -> Option<&Returning> {
    match stmt {
        Statement::Insert(i) => i.returning.as_ref(),
        Statement::Update(u) => u.returning.as_ref(),
        Statement::Delete(d) => d.returning.as_ref(),
        _ => None,
    }
}

/// Applies CREATE or ALTER SEQUENCE options to the defaults or an existing definition.
fn sequence_def(
    old: Option<&SequenceDef>,
    options: &[SequenceOption],
) -> Result<SequenceDef, OraError> {
    const MAX: i128 = 9_999_999_999_999_999_999_999_999_999; // 28 nines
    const MIN: i128 = -999_999_999_999_999_999_999_999_999; // 27 nines
    let mut increment = old.map_or(1, |d| d.increment);
    let mut start = None;
    let mut cycle = old.is_some_and(|d| d.cycle);
    let mut min_set = old.map(|d| Some(d.min));
    let mut max_set = old.map(|d| Some(d.max));
    for o in options {
        match o {
            SequenceOption::StartWith(n) => {
                if old.is_some() {
                    return Err(OraError::new(2283, "cannot alter starting sequence number"));
                }
                start = Some(*n)
            }
            SequenceOption::IncrementBy(n) => {
                if *n == 0 {
                    return Err(OraError::new(4002, "INCREMENT must be a non-zero integer"));
                }
                increment = *n
            }
            SequenceOption::MinValue(n) => min_set = Some(*n),
            SequenceOption::MaxValue(n) => max_set = Some(*n),
            SequenceOption::Cycle(c) => cycle = *c,
            SequenceOption::Restart(_) => {}
        }
    }
    let min = min_set
        .flatten()
        .unwrap_or(if increment > 0 { 1 } else { MIN });
    let max = max_set
        .flatten()
        .unwrap_or(if increment > 0 { MAX } else { -1 });
    if min >= max {
        return Err(OraError::new(4028, "cannot generate internal sequence"));
    }
    let start = match old {
        Some(d) => d.start,
        None => start.unwrap_or(if increment > 0 { min } else { max }),
    };
    if old.is_none() {
        if start < min {
            return Err(OraError::new(
                4006,
                "START WITH cannot be less than MINVALUE",
            ));
        }
        if start > max {
            return Err(OraError::new(
                4008,
                "START WITH cannot be more than MAXVALUE",
            ));
        }
    }
    Ok(SequenceDef {
        start,
        increment,
        min,
        max,
        cycle,
    })
}

/// Reverts the log entries after `keep`, newest first. The rows of one UPDATE are
/// reverted together so swapped unique keys do not collide on the way back.
fn undo(txn: &mut Txn, keep: usize) {
    let mut changes = txn.log.split_off(keep);
    while let Some(change) = changes.pop() {
        match change {
            Change::Insert { table, id, .. } => {
                if let Some(t) = txn.work.table_mut(&table) {
                    t.delete(id);
                }
            }
            Change::Delete { table, id, old } => {
                if let Some(t) = txn.work.table_mut(&table) {
                    let _ = t.insert(id, old);
                }
            }
            Change::Update { table, id, old, .. } => {
                let mut batch = vec![(id, old)];
                while let Some(Change::Update { table: next, .. }) = changes.last() {
                    if *next != table {
                        break;
                    }
                    if let Some(Change::Update { id, old, .. }) = changes.pop() {
                        batch.push((id, old));
                    }
                }
                if let Some(t) = txn.work.table_mut(&table) {
                    let _ = t.update(batch);
                }
            }
        }
    }
}

fn parse_time_zone(value: &str) -> Result<i32, OraError> {
    let invalid = || OraError::new(1882, "timezone region not found");
    let v = value.trim().to_uppercase();
    match v.as_str() {
        "UTC" | "GMT" | "DBTIMEZONE" | "Z" => return Ok(0),
        "LOCAL" | "SESSIONTIMEZONE" => {
            return Ok(chrono::Local::now().offset().local_minus_utc() / 60)
        }
        _ => {}
    }
    let (sign, rest) = match v.as_bytes().first() {
        Some(b'+') => (1, &v[1..]),
        Some(b'-') => (-1, &v[1..]),
        _ => return Err(invalid()),
    };
    let (h, m) = rest.split_once(':').unwrap_or((rest, "0"));
    let h: i32 = h.trim().parse().map_err(|_| invalid())?;
    let m: i32 = m.trim().parse().map_err(|_| invalid())?;
    if !(0..=14).contains(&h) || !(0..60).contains(&m) {
        return Err(OraError::new(
            1874,
            "time zone hour must be between -15 and 15",
        ));
    }
    Ok(sign * (h * 60 + m))
}

fn table_scope_cols(t: &Table, qualifier: &str) -> Vec<RelCol> {
    t.columns
        .iter()
        .map(|c| RelCol {
            qualifier: Some(qualifier.to_string()),
            name: c.name.clone(),
            sql_type: c.sql_type,
        })
        .collect()
}

/// Converts a value for storage in a column, enforcing the column's type, length and precision.
pub(crate) fn store_value(
    env: &Env,
    user: &str,
    table: &str,
    col: &TableColumn,
    v: Value,
) -> Result<Value, OraError> {
    if v.is_null() {
        return Ok(Value::Null);
    }
    if let Value::Boolean(_) = v {
        return Err(OraError::inconsistent(&type_name(col.sql_type), "BOOLEAN"));
    }
    let too_large = |actual: usize, max: u32| {
        OraError::new(
            12899,
            format!("value too large for column \"{user}\".\"{table}\".\"{}\" (actual: {actual}, maximum: {max})", col.name),
        )
    };
    Ok(match col.sql_type {
        SqlType::Number { precision, scale } => {
            let n = match &v {
                Value::Date(_) | Value::Timestamp(_) => {
                    return Err(OraError::inconsistent("NUMBER", "DATE"))
                }
                v => v.to_number()?.unwrap(),
            };
            if precision == 0 {
                return Ok(Value::Number(n));
            }
            let n = n
                .with_scale_round(scale as i64, RoundingMode::HalfUp)
                .normalized();
            // Digits allowed before the decimal point: precision - scale.
            let int_digits = {
                let int = n.abs().with_scale_round(0, RoundingMode::Down);
                if int == 0 {
                    0
                } else {
                    int.to_string().trim_start_matches('-').len() as i64
                }
            };
            if int_digits > precision as i64 - scale as i64 {
                return Err(OraError::new(
                    1438,
                    "value larger than specified precision allowed for this column",
                ));
            }
            Value::Number(n)
        }
        SqlType::Varchar2(max) | SqlType::Char(max) => {
            let s = match &v {
                Value::Varchar2(s) => s.clone(),
                Value::Number(n) => crate::format_number(n),
                Value::Date(d) => datetime::format(d, &env.nls_date_format),
                Value::Timestamp(d) => datetime::format(d, &env.nls_timestamp_format),
                Value::Null | Value::Boolean(_) => unreachable!(),
            };
            let s = if let SqlType::Char(n) = col.sql_type {
                let pad = (n as usize).saturating_sub(s.len());
                s + &" ".repeat(pad)
            } else {
                s
            };
            if s.len() > max as usize {
                return Err(too_large(s.len(), max));
            }
            Value::varchar(s)
        }
        SqlType::Date => match v {
            Value::Number(_) => return Err(OraError::inconsistent("DATE", "NUMBER")),
            v => {
                use chrono::Timelike;
                let d = v.to_datetime(&env.nls_date_format)?.unwrap();
                Value::Date(d.with_nanosecond(0).unwrap())
            }
        },
        SqlType::Timestamp(p) => match v {
            Value::Number(_) => return Err(OraError::inconsistent("TIMESTAMP", "NUMBER")),
            Value::Varchar2(_) => Value::Timestamp(round_fraction(
                v.to_datetime(&env.nls_timestamp_format)?.unwrap(),
                p,
            )),
            v => Value::Timestamp(round_fraction(v.to_datetime("")?.unwrap(), p)),
        },
    })
}

/// The type name Oracle uses in messages.
pub(crate) fn type_name(t: SqlType) -> String {
    match t {
        SqlType::Number { .. } => "NUMBER".into(),
        SqlType::Varchar2(_) => "VARCHAR2".into(),
        SqlType::Char(_) => "CHAR".into(),
        SqlType::Date => "DATE".into(),
        SqlType::Timestamp(_) => "TIMESTAMP".into(),
    }
}

fn round_fraction(d: chrono::NaiveDateTime, precision: u8) -> chrono::NaiveDateTime {
    use chrono::Timelike;
    let unit = 10u32.pow(9 - precision.min(9) as u32);
    let nanos = d.nanosecond();
    let rounded = (nanos + unit / 2) / unit * unit;
    if rounded >= 1_000_000_000 {
        d.with_nanosecond(0).unwrap() + chrono::Duration::seconds(1)
    } else {
        d.with_nanosecond(rounded).unwrap()
    }
}

/// Checks NOT NULL and CHECK constraints for a full row.
fn check_row(ex: &Ex, t: &Table, values: &[Value], updating: bool) -> Result<(), OraError> {
    let user = &ex.env.user;
    for (c, v) in t.columns.iter().zip(values) {
        if c.not_null && v.is_null() {
            let target = format!("(\"{user}\".\"{}\".\"{}\")", t.name, c.name);
            return Err(if updating {
                OraError::new(1407, format!("cannot update {target} to NULL"))
            } else {
                OraError::new(1400, format!("cannot insert NULL into {target}"))
            });
        }
    }
    let cols = table_scope_cols(t, &t.name);
    for c in &t.constraints {
        if let ConstraintKind::Check(e) = &c.kind {
            if ex.eval_bool(e, &Scope::row(&cols, values, None))? == Some(false) {
                return Err(OraError::new(
                    2290,
                    format!("check constraint ({user}.{}) violated", c.name),
                ));
            }
        }
    }
    Ok(())
}

/// What a DML statement runs with.
struct Dml<'a> {
    db: &'a Database,
    env: &'a Env,
    binds: &'a [Value],
    vars: Option<&'a dyn VarLookup>,
}

impl<'a> Dml<'a> {
    fn ex(&self, cat: &'a Catalog) -> Ex<'a> {
        Ex {
            cat,
            binds: self.binds,
            env: self.env,
            db: self.db,
            vars: self.vars,
        }
    }
}

/// Evaluates RETURNING expressions over the rows a statement changed.
fn returning_rows(
    ex: &Ex,
    returning: &Option<Returning>,
    cols: &[RelCol],
    rows: &[&[Value]],
) -> Result<Vec<Vec<Value>>, OraError> {
    let Some(r) = returning else {
        return Ok(Vec::new());
    };
    rows.iter()
        .map(|row| {
            r.exprs
                .iter()
                .map(|e| ex.eval(e, &Scope::row(cols, row, None)))
                .collect()
        })
        .collect()
}

type DmlResult = Result<(u64, Vec<Vec<Value>>), OraError>;

fn insert(ctx: &Dml, txn: &mut Txn, ins: &Insert) -> DmlResult {
    let (db, env) = (ctx.db, ctx.env);
    let ex = ctx.ex(&txn.work);
    let t = txn
        .work
        .table(&ins.table)
        .ok_or_else(OraError::table_not_found)?;
    // Target column positions.
    let targets: Vec<usize> = match &ins.columns {
        Some(names) => {
            let mut seen = HashSet::new();
            let mut out = Vec::new();
            for n in names {
                let i = t
                    .column_index(n)
                    .ok_or_else(|| OraError::invalid_identifier(&format!("\"{n}\"")))?;
                if !seen.insert(i) {
                    return Err(OraError::new(957, "duplicate column name"));
                }
                out.push(i);
            }
            out
        }
        None => (0..t.columns.len()).collect(),
    };
    // Defaults of the columns the statement leaves out.
    let defaults: Vec<&Expr> = t
        .columns
        .iter()
        .enumerate()
        .filter(|(i, _)| !targets.contains(i))
        .filter_map(|(_, c)| c.default.as_ref())
        .collect();
    // A VALUES list is one row; its sequence values also serve the defaults.
    let values_row = match &ins.source {
        InsertSource::Values(exprs) => {
            Some(ex.start_row(exprs.iter().chain(defaults.iter().copied()))?)
        }
        InsertSource::Query(_) => None,
    };
    let sources: Vec<Vec<Value>> = match &ins.source {
        InsertSource::Values(exprs) => {
            if exprs.len() > targets.len() {
                return Err(OraError::new(913, "too many values"));
            }
            if exprs.len() < targets.len() {
                return Err(OraError::new(947, "not enough values"));
            }
            let mut row = Vec::with_capacity(exprs.len());
            for e in exprs {
                row.push(ex.eval(e, &Scope::empty(None))?);
            }
            vec![row]
        }
        InsertSource::Query(q) => {
            let rel = ex.query(q, None)?;
            if rel.cols.len() > targets.len() {
                return Err(OraError::new(913, "too many values"));
            }
            if rel.cols.len() < targets.len() {
                return Err(OraError::new(947, "not enough values"));
            }
            rel.rows
        }
    };
    let mut rows = Vec::with_capacity(sources.len());
    for src in sources {
        let _row = match values_row {
            Some(_) => None,
            None => Some(ex.start_row(defaults.iter().copied())?),
        };
        let mut values = vec![None; t.columns.len()];
        for (&i, v) in targets.iter().zip(src) {
            values[i] = Some(v);
        }
        let mut full = Vec::with_capacity(values.len());
        for (col, v) in t.columns.iter().zip(values) {
            let v = match v {
                Some(v) if !(col.identity && v.is_null()) => v,
                _ if col.identity => Value::number(db.next_identity(&t.name)),
                Some(v) => v,
                None => match &col.default {
                    Some(d) => ex.eval(d, &Scope::empty(None))?,
                    None => Value::Null,
                },
            };
            full.push(store_value(env, &env.user, &t.name, col, v)?);
        }
        check_row(&ex, t, &full, false)?;
        rows.push(full);
    }
    drop(values_row);
    let cols = table_scope_cols(t, &t.name);
    let returned = returning_rows(
        &ex,
        &ins.returning,
        &cols,
        &rows.iter().map(Vec::as_slice).collect::<Vec<_>>(),
    )?;
    let count = rows.len() as u64;
    let table = txn.work.table_mut(&ins.table).unwrap();
    for values in rows {
        let id = db.next_row_id();
        table
            .insert(id, values.clone())
            .map_err(|c| unique_violation(&env.user, &c))?;
        txn.log.push(Change::Insert {
            table: ins.table.clone(),
            id,
            values,
        });
    }
    Ok((count, returned))
}

/// The ids and values of the rows a WHERE clause selects.
fn matching_rows(
    ex: &Ex,
    t: &Table,
    cols: &[RelCol],
    where_: &Option<Expr>,
) -> Result<Vec<(u64, Vec<Value>)>, OraError> {
    let mut out = Vec::new();
    for (id, row) in &t.rows {
        let keep = match where_ {
            Some(w) => {
                let scope = Scope {
                    rownum: out.len() as i64 + 1,
                    ..Scope::row(cols, row, None)
                };
                ex.eval_bool(w, &scope)? == Some(true)
            }
            None => true,
        };
        if keep {
            out.push((*id, row.clone()));
        }
    }
    Ok(out)
}

fn update(ctx: &Dml, txn: &mut Txn, upd: &Update) -> DmlResult {
    let env = ctx.env;
    let ex = ctx.ex(&txn.work);
    let t = txn
        .work
        .table(&upd.table)
        .ok_or_else(OraError::table_not_found)?;
    let cols = table_scope_cols(t, upd.alias.as_deref().unwrap_or(&upd.table));
    let mut targets = Vec::new();
    for (name, _) in &upd.assignments {
        let i = t
            .column_index(name)
            .ok_or_else(|| OraError::invalid_identifier(&format!("\"{name}\"")))?;
        if targets.contains(&i) {
            return Err(OraError::new(
                1747,
                "invalid user.table.column, table.column, or column specification",
            ));
        }
        targets.push(i);
    }
    let mut changes = Vec::new();
    let mut olds = Vec::new();
    for (id, old) in matching_rows(&ex, t, &cols, &upd.where_)? {
        let _row = ex.start_row(upd.assignments.iter().map(|(_, e)| e))?;
        let mut new = old.clone();
        for ((_, e), &i) in upd.assignments.iter().zip(&targets) {
            let v = ex.eval(e, &Scope::row(&cols, &old, None))?;
            new[i] = store_value(env, &env.user, &t.name, &t.columns[i], v)?;
        }
        check_row(&ex, t, &new, true)?;
        changes.push((id, new));
        olds.push(old);
    }
    let returned = returning_rows(
        &ex,
        &upd.returning,
        &cols,
        &changes
            .iter()
            .map(|(_, v)| v.as_slice())
            .collect::<Vec<_>>(),
    )?;
    let count = changes.len() as u64;
    let table = txn.work.table_mut(&upd.table).unwrap();
    table
        .update(changes.clone())
        .map_err(|c| unique_violation(&env.user, &c))?;
    for ((id, values), old) in changes.into_iter().zip(olds) {
        txn.log.push(Change::Update {
            table: upd.table.clone(),
            id,
            values,
            old,
        });
    }
    Ok((count, returned))
}

fn delete(ctx: &Dml, txn: &mut Txn, del: &Delete) -> DmlResult {
    let ex = ctx.ex(&txn.work);
    let t = txn
        .work
        .table(&del.table)
        .ok_or_else(OraError::table_not_found)?;
    let cols = table_scope_cols(t, del.alias.as_deref().unwrap_or(&del.table));
    let rows = matching_rows(&ex, t, &cols, &del.where_)?;
    let returned = returning_rows(
        &ex,
        &del.returning,
        &cols,
        &rows.iter().map(|(_, v)| v.as_slice()).collect::<Vec<_>>(),
    )?;
    let count = rows.len() as u64;
    let table = txn.work.table_mut(&del.table).unwrap();
    for (id, old) in rows {
        table.delete(id);
        txn.log.push(Change::Delete {
            table: del.table.clone(),
            id,
            old,
        });
    }
    Ok((count, returned))
}
