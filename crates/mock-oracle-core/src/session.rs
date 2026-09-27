//! Sessions: statement execution, transactions, DML and DDL.

use std::collections::HashSet;
use std::sync::Arc;

use bigdecimal::RoundingMode;

use crate::ast::*;
use crate::catalog::SequenceDef;
use crate::catalog::{foreign_key, Catalog, Constraint, ConstraintKind, Table, TableColumn};
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
        }
        .and_then(|r| enforce_foreign_keys(&self.env, txn, saved).map(|_| r));
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
                state.tables.insert(
                    ct.name.clone(),
                    Arc::new(Table::new(ct.name.clone(), columns)),
                );
                // System names follow the declaration order; foreign keys are added last so
                // that they can reference this table's own keys.
                let mut constraints: Vec<_> = ct
                    .constraints
                    .into_iter()
                    .map(|mut c| {
                        c.name.get_or_insert_with(|| self.db.constraint_name());
                        c
                    })
                    .collect();
                constraints.sort_by_key(|c| {
                    matches!(c.kind, crate::ast::ConstraintKind::ForeignKey { .. })
                });
                for c in constraints {
                    if let Err(e) = add_constraint(&self.env, &self.db, &mut state, &ct.name, c) {
                        state.tables.remove(&ct.name);
                        return Err(e);
                    }
                }
                let table = state.table_mut(&ct.name).unwrap();
                for values in rows {
                    let id = self.db.next_row_id();
                    if let Err(c) = table.insert(id, values) {
                        state.tables.remove(&ct.name);
                        return Err(unique_violation(&user, &c));
                    }
                }
                self.db.identities.lock().unwrap().remove(&ct.name);
                state.version += 1;
            }
            Statement::DropTable {
                name,
                cascade_constraints,
            } => {
                let mut state = self.db.state.write().unwrap();
                if !state.tables.contains_key(&name) {
                    return Err(OraError::table_not_found());
                }
                let refs: Vec<_> = state
                    .foreign_keys()
                    .filter(|f| f.parent == name && f.child != name)
                    .collect();
                if !refs.is_empty() && !cascade_constraints {
                    return Err(OraError::new(
                        2449,
                        "unique/primary keys in table referenced by foreign keys",
                    ));
                }
                for f in refs {
                    let child = state.table_mut(&f.child).unwrap();
                    child.constraints.retain(|c| c.is_index || c.name != f.name);
                }
                state.tables.remove(&name);
                self.db.identities.lock().unwrap().remove(&name);
                state.version += 1;
            }
            Statement::Truncate { name } => {
                let mut state = self.db.state.write().unwrap();
                if state
                    .foreign_keys()
                    .any(|f| f.parent == name && f.child != name && f.enabled)
                {
                    return Err(OraError::new(
                        2266,
                        "unique/primary keys in table referenced by enabled foreign keys",
                    ));
                }
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
                    t.add_unique(name, idx, false, true, true).map_err(|_| {
                        OraError::new(1452, "cannot CREATE UNIQUE INDEX; duplicate keys found")
                    })?;
                } else {
                    t.constraints.push(Constraint {
                        name,
                        kind: ConstraintKind::Index,
                        is_index: true,
                        enabled: true,
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
            Statement::AlterTable { name, action } => {
                // Work on a copy, so that validating a constraint over the existing rows
                // runs without holding the lock; retry if another session changed the state.
                loop {
                    let mut cat = self.db.committed();
                    alter_table(&self.env, &self.db, &mut cat, &name, &action)?;
                    let mut state = self.db.state.write().unwrap();
                    if state.version == cat.version {
                        cat.version += 1;
                        *state = cat;
                        break;
                    }
                }
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
            Statement::PartitionMaintenance { table } => {
                if let Some(name) = table {
                    let state = self.db.state.read().unwrap();
                    if !state.tables.contains_key(&name) {
                        return Err(OraError::table_not_found());
                    }
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
    // Primary key columns are NOT NULL while the key is enabled.
    let key: &[usize] = t
        .constraints
        .iter()
        .find_map(|c| match &c.kind {
            ConstraintKind::Unique {
                columns,
                primary: true,
                ..
            } if c.enabled => Some(columns.as_slice()),
            _ => None,
        })
        .unwrap_or_default();
    for (i, (c, v)) in t.columns.iter().zip(values).enumerate() {
        if (c.not_null || key.contains(&i)) && v.is_null() {
            let target = format!("(\"{user}\".\"{}\".\"{}\")", t.name, c.name);
            return Err(if updating {
                OraError::new(1407, format!("cannot update {target} to NULL"))
            } else {
                OraError::new(1400, format!("cannot insert NULL into {target}"))
            });
        }
    }
    let cols = table_scope_cols(t, &t.name);
    for c in t.constraints.iter().filter(|c| c.enabled) {
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

// ---- constraints ----

fn resolve_columns(t: &Table, names: &[String]) -> Result<Vec<usize>, OraError> {
    names
        .iter()
        .map(|n| {
            t.column_index(n)
                .ok_or_else(|| OraError::invalid_identifier(&format!("\"{n}\"")))
        })
        .collect()
}

/// Which types a foreign key column may pair with.
fn type_family(t: SqlType) -> u8 {
    match t {
        SqlType::Number { .. } => 0,
        SqlType::Varchar2(_) | SqlType::Char(_) => 1,
        SqlType::Date => 2,
        SqlType::Timestamp(_) => 3,
    }
}

/// Adds a constraint to `table`, checking the rows already there unless it is disabled or
/// NOVALIDATE. On failure `cat` may be left half changed, so callers discard it.
fn add_constraint(
    env: &Env,
    db: &Database,
    cat: &mut Catalog,
    table: &str,
    c: crate::ast::TableConstraint,
) -> Result<(), OraError> {
    use crate::ast::ConstraintKind as K;
    let user = &env.user;
    let name = match c.name {
        Some(n) => n,
        None => db.constraint_name(),
    };
    if cat
        .tables
        .values()
        .any(|t| t.constraints.iter().any(|k| !k.is_index && k.name == name))
    {
        return Err(OraError::new(
            2264,
            "name already used by an existing constraint",
        ));
    }
    let t = cat.table(table).ok_or_else(OraError::table_not_found)?;
    let kind = match c.kind {
        K::PrimaryKey(cols) => {
            if t.has_primary_key() {
                return Err(OraError::new(2260, "table can have only one primary key"));
            }
            let idx = resolve_columns(t, &cols)?;
            if c.enabled && t.rows.values().any(|r| idx.iter().any(|&i| r[i].is_null())) {
                return Err(OraError::new(
                    1449,
                    "column contains NULL values; cannot alter to NOT NULL",
                ));
            }
            cat.table_mut(table)
                .unwrap()
                .add_unique(name.clone(), idx, true, false, c.enabled)
                .map_err(|_| {
                    OraError::new(
                        2437,
                        format!("cannot validate ({user}.{name}) - primary key violated"),
                    )
                })?;
            return Ok(());
        }
        K::Unique(cols) => {
            let idx = resolve_columns(t, &cols)?;
            cat.table_mut(table)
                .unwrap()
                .add_unique(name.clone(), idx, false, false, c.enabled)
                .map_err(|_| {
                    OraError::new(
                        2299,
                        format!("cannot validate ({user}.{name}) - duplicate keys found"),
                    )
                })?;
            return Ok(());
        }
        K::Check(e) => ConstraintKind::Check(e),
        K::ForeignKey {
            columns,
            table: parent,
            ref_columns,
            on_delete,
        } => {
            let idx = resolve_columns(t, &columns)?;
            let p = cat.table(&parent).ok_or_else(OraError::table_not_found)?;
            let refs = if ref_columns.is_empty() {
                p.constraints
                    .iter()
                    .find_map(|k| match &k.kind {
                        ConstraintKind::Unique {
                            columns,
                            primary: true,
                            ..
                        } => Some(columns.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| {
                        OraError::new(2268, "referenced table does not have a primary key")
                    })?
            } else {
                resolve_columns(p, &ref_columns)?
            };
            if refs.len() != idx.len() {
                return Err(OraError::new(
                    2256,
                    "number of referencing columns must match referenced columns",
                ));
            }
            // The referenced columns must be exactly those of a primary key or unique
            // constraint, in any order.
            let key = p
                .constraints
                .iter()
                .filter(|k| !k.is_index)
                .find_map(|k| match &k.kind {
                    ConstraintKind::Unique { columns, .. }
                        if columns.len() == refs.len()
                            && columns.iter().all(|c| refs.contains(c)) =>
                    {
                        Some(columns.clone())
                    }
                    _ => None,
                })
                .ok_or_else(|| {
                    OraError::new(
                        2270,
                        "no matching unique or primary key for this column-list",
                    )
                })?;
            // Line this table's columns up with the key's column order.
            let columns: Vec<usize> = key
                .iter()
                .map(|k| idx[refs.iter().position(|r| r == k).unwrap()])
                .collect();
            for (&c, &k) in columns.iter().zip(&key) {
                if type_family(t.columns[c].sql_type) != type_family(p.columns[k].sql_type) {
                    return Err(OraError::new(
                        2267,
                        "column type incompatible with referenced column type",
                    ));
                }
            }
            ConstraintKind::ForeignKey {
                columns,
                parent,
                parent_columns: key,
                on_delete,
            }
        }
    };
    let t = cat.table_mut(table).unwrap();
    t.constraints.push(Constraint {
        name,
        kind,
        is_index: false,
        enabled: c.enabled,
    });
    let i = t.constraints.len() - 1;
    if c.enabled && c.validate {
        validate_constraint(env, db, cat, table, i)?;
    }
    Ok(())
}

/// Checks the rows already in `table` against its CHECK or FOREIGN KEY constraint at
/// position `i`.
fn validate_constraint(
    env: &Env,
    db: &Database,
    cat: &Catalog,
    table: &str,
    i: usize,
) -> Result<(), OraError> {
    let t = cat.table(table).unwrap();
    let c = &t.constraints[i];
    let cannot = |code: u32, why: &str| {
        OraError::new(
            code,
            format!("cannot validate ({}.{}) - {why}", env.user, c.name),
        )
    };
    match &c.kind {
        ConstraintKind::Check(e) => {
            let ex = Ex {
                cat,
                binds: &[],
                env,
                db,
                vars: None,
            };
            let cols = table_scope_cols(t, &t.name);
            for r in t.rows.values() {
                if ex.eval_bool(e, &Scope::row(&cols, r, None))? == Some(false) {
                    return Err(cannot(2293, "check constraint violated"));
                }
            }
        }
        ConstraintKind::ForeignKey {
            columns,
            parent,
            parent_columns,
            ..
        } => {
            let p = cat.table(parent).ok_or_else(OraError::table_not_found)?;
            for r in t.rows.values() {
                if let Some(k) = foreign_key(columns, r) {
                    if !p.has_key(parent_columns, &k) {
                        return Err(cannot(2298, "parent keys not found"));
                    }
                }
            }
        }
        ConstraintKind::Unique {
            columns,
            primary: true,
            ..
        } => {
            if t.rows
                .values()
                .any(|r| columns.iter().any(|&i| r[i].is_null()))
            {
                return Err(OraError::new(
                    1449,
                    "column contains NULL values; cannot alter to NOT NULL",
                ));
            }
        }
        _ => {}
    }
    Ok(())
}

/// The position of a table's constraint by name, or of its primary key when `name` is `None`.
fn find_constraint(t: &Table, name: Option<&str>) -> Option<usize> {
    t.constraints.iter().position(|c| {
        !c.is_index
            && match name {
                Some(n) => c.name == n,
                None => matches!(c.kind, ConstraintKind::Unique { primary: true, .. }),
            }
    })
}

/// Applies an ALTER TABLE constraint change to `cat`.
fn alter_table(
    env: &Env,
    db: &Database,
    cat: &mut Catalog,
    table: &str,
    action: &AlterTableAction,
) -> Result<(), OraError> {
    let user = &env.user;
    let t = cat.table(table).ok_or_else(OraError::table_not_found)?;
    match action {
        AlterTableAction::AddConstraints(cs) => {
            for c in cs {
                add_constraint(env, db, cat, table, c.clone())?;
            }
        }
        AlterTableAction::DropConstraint { name, cascade } => {
            let i = find_constraint(t, name.as_deref()).ok_or_else(|| match name {
                Some(_) => OraError::new(2443, "Cannot drop constraint  - nonexistent constraint"),
                None => OraError::new(2441, "Cannot drop nonexistent primary key"),
            })?;
            if let ConstraintKind::Unique { columns, .. } = &t.constraints[i].kind {
                let refs = cat.referencing(table, columns);
                if !refs.is_empty() && !cascade {
                    return Err(OraError::new(
                        2273,
                        "this unique/primary key is referenced by some foreign keys",
                    ));
                }
                for f in refs {
                    let child = cat.table_mut(&f.child).unwrap();
                    child.constraints.retain(|c| c.is_index || c.name != f.name);
                }
            }
            let t = cat.table_mut(table).unwrap();
            let name = t.constraints[i].name.clone();
            t.constraints.retain(|c| c.is_index || c.name != name);
        }
        AlterTableAction::SetConstraint {
            name,
            enabled,
            validate,
            cascade,
        } => {
            let i = find_constraint(t, name.as_deref()).ok_or_else(|| match name {
                Some(name) => {
                    let what = if *enabled {
                        (2430, "enable")
                    } else {
                        (2431, "disable")
                    };
                    OraError::new(
                        what.0,
                        format!(
                            "cannot {} constraint ({user}.{name}) - no such constraint",
                            what.1
                        ),
                    )
                }
                None => OraError::new(
                    2432,
                    "cannot enable primary key - primary key not defined for table",
                ),
            })?;
            let name = t.constraints[i].name.clone();
            if !enabled {
                if let ConstraintKind::Unique { columns, .. } = &t.constraints[i].kind {
                    let refs: Vec<_> = cat
                        .referencing(table, columns)
                        .into_iter()
                        .filter(|f| f.enabled)
                        .collect();
                    if !refs.is_empty() && !cascade {
                        return Err(OraError::new(
                            2297,
                            format!(
                                "cannot disable constraint ({user}.{name}) - dependencies exist"
                            ),
                        ));
                    }
                    for f in refs {
                        let child = cat.table_mut(&f.child).unwrap();
                        let j = child
                            .constraints
                            .iter()
                            .position(|c| !c.is_index && c.name == f.name)
                            .unwrap();
                        let _ = child.set_enabled(j, false);
                    }
                }
            }
            let t = cat.table_mut(table).unwrap();
            let primary = matches!(
                t.constraints[i].kind,
                ConstraintKind::Unique { primary: true, .. }
            );
            let was_enabled = t.constraints[i].enabled;
            t.set_enabled(i, *enabled).map_err(|_| {
                let why = if primary {
                    (2437, "primary key violated")
                } else {
                    (2299, "duplicate keys found")
                };
                OraError::new(
                    why.0,
                    format!("cannot validate ({user}.{name}) - {}", why.1),
                )
            })?;
            if *enabled && (*validate || primary) && !was_enabled {
                validate_constraint(env, db, cat, table, i)?;
            }
        }
    }
    Ok(())
}

/// Enforces the foreign keys after a DML statement, whose changes start at `from` in the
/// log. Removed parent keys fail (ORA-02292) or cascade to their child rows; new or
/// changed child rows need an existing parent key (ORA-02291). Checking once the whole
/// statement has run lets one statement insert or delete a parent together with its
/// children, as Oracle does.
fn enforce_foreign_keys(env: &Env, txn: &mut Txn, from: usize) -> Result<(), OraError> {
    let user = &env.user;
    let fks: Vec<_> = txn.work.foreign_keys().filter(|f| f.enabled).collect();
    if fks.is_empty() {
        return Ok(());
    }
    let violated = |code: u32, name: &str, why: &str| {
        OraError::new(
            code,
            format!("integrity constraint ({user}.{name}) violated - {why}"),
        )
    };
    // Parent side. Cascaded deletes remove further keys, so this runs in rounds over
    // the changes the previous round logged.
    let mut start = from;
    while start < txn.log.len() {
        let end = txn.log.len();
        for f in &fks {
            let mut deleted = HashSet::new();
            let mut updated = HashSet::new();
            for change in &txn.log[start..end] {
                match change {
                    Change::Delete { table, old, .. } if *table == f.parent => {
                        deleted.extend(foreign_key(&f.parent_columns, old));
                    }
                    Change::Update {
                        table, old, values, ..
                    } if *table == f.parent => {
                        let old = foreign_key(&f.parent_columns, old);
                        if old.is_some() && old != foreign_key(&f.parent_columns, values) {
                            updated.extend(old);
                        }
                    }
                    _ => {}
                }
            }
            // A key another row still holds is still there for its children.
            let parent = txn.work.table(&f.parent).unwrap();
            deleted.retain(|k| !parent.has_key(&f.parent_columns, k));
            updated.retain(|k| !parent.has_key(&f.parent_columns, k));
            if deleted.is_empty() && updated.is_empty() {
                continue;
            }
            let child = txn.work.table(&f.child).unwrap();
            let mut hits = Vec::new();
            for (id, row) in &child.rows {
                if let Some(k) = foreign_key(&f.columns, row) {
                    if updated.contains(&k) {
                        return Err(violated(2292, &f.name, "child record found"));
                    }
                    if deleted.contains(&k) {
                        hits.push((*id, row.clone()));
                    }
                }
            }
            if hits.is_empty() {
                continue;
            }
            match f.on_delete {
                OnDelete::Restrict => {
                    return Err(violated(2292, &f.name, "child record found"));
                }
                OnDelete::Cascade => {
                    let child = txn.work.table_mut(&f.child).unwrap();
                    for (id, old) in hits {
                        child.delete(id);
                        txn.log.push(Change::Delete {
                            table: f.child.clone(),
                            id,
                            old,
                        });
                    }
                }
                OnDelete::SetNull => {
                    let key: &[usize] = child
                        .constraints
                        .iter()
                        .find_map(|c| match &c.kind {
                            ConstraintKind::Unique {
                                columns,
                                primary: true,
                                ..
                            } if c.enabled => Some(columns.as_slice()),
                            _ => None,
                        })
                        .unwrap_or_default();
                    for &c in &f.columns {
                        if child.columns[c].not_null || key.contains(&c) {
                            return Err(OraError::new(
                                1407,
                                format!(
                                    "cannot update (\"{user}\".\"{}\".\"{}\") to NULL",
                                    child.name, child.columns[c].name
                                ),
                            ));
                        }
                    }
                    let changes: Vec<_> = hits
                        .iter()
                        .map(|(id, old)| {
                            let mut new = old.clone();
                            for &c in &f.columns {
                                new[c] = Value::Null;
                            }
                            (*id, new)
                        })
                        .collect();
                    txn.work
                        .table_mut(&f.child)
                        .unwrap()
                        .update(changes.clone())
                        .map_err(|c| unique_violation(user, &c))?;
                    for ((id, values), (_, old)) in changes.into_iter().zip(hits) {
                        txn.log.push(Change::Update {
                            table: f.child.clone(),
                            id,
                            values,
                            old,
                        });
                    }
                }
            }
        }
        start = end;
    }
    // Child side: each row the statement inserted or changed needs its parent key.
    for change in &txn.log[from..] {
        let (Change::Insert { table, id, .. } | Change::Update { table, id, .. }) = change else {
            continue;
        };
        let Some(row) = txn.work.table(table).and_then(|t| t.rows.get(id)) else {
            continue;
        };
        for f in fks.iter().filter(|f| f.child == *table) {
            if let Some(k) = foreign_key(&f.columns, row) {
                let found = txn
                    .work
                    .table(&f.parent)
                    .is_some_and(|p| p.has_key(&f.parent_columns, &k));
                if !found {
                    return Err(violated(2291, &f.name, "parent key not found"));
                }
            }
        }
    }
    Ok(())
}
