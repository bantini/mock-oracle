//! Sessions: statement execution, transactions, DML and DDL.

use std::collections::HashSet;
use std::sync::Arc;

use bigdecimal::RoundingMode;

use crate::ast::*;
use crate::catalog::{Catalog, ConstraintKind, Table, TableColumn};
use crate::eval::{infer_type, Env, Ex, RelCol, Scope};
use crate::{datetime, parser, Column, Database, OraError, QueryResult, SqlType, Value};

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
    },
    Delete {
        table: String,
        id: u64,
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

/// A connection's view of a [`Database`]. Changes are private to the session until
/// [`Session::commit`]; dropping the session rolls them back.
#[derive(Debug)]
pub struct Session {
    db: Arc<Database>,
    env: Env,
    txn: Option<Txn>,
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

    /// Executes one SQL statement. `binds` are the bind values in the order the
    /// placeholders appear in the statement.
    pub fn execute(&mut self, sql: &str, binds: &[Value]) -> Result<QueryResult, OraError> {
        let parsed = parser::parse(sql)?;
        if binds.len() < parsed.binds {
            return Err(OraError::new(1008, "not all variables bound"));
        }
        self.env.start_statement();
        match parsed.stmt {
            Statement::Query(q) => {
                let committed;
                let cat = match &self.txn {
                    Some(t) => &t.work,
                    None => {
                        committed = self.db.committed();
                        &committed
                    }
                };
                let rel = Ex {
                    cat,
                    binds,
                    env: &self.env,
                }
                .query(&q, None)?;
                Ok(QueryResult {
                    is_query: true,
                    columns: rel
                        .cols
                        .into_iter()
                        .map(|c| Column {
                            name: c.name,
                            sql_type: c.sql_type,
                        })
                        .collect(),
                    rows: rel.rows,
                    rows_affected: 0,
                })
            }
            Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                let rows_affected = self.dml(&parsed.stmt, binds)?;
                Ok(QueryResult {
                    rows_affected,
                    ..Default::default()
                })
            }
            Statement::Commit => self.commit().map(|_| QueryResult::default()),
            Statement::Rollback => {
                self.rollback();
                Ok(QueryResult::default())
            }
            Statement::AlterSession { name, value } => {
                self.alter_session(&name, &value)?;
                Ok(QueryResult::default())
            }
            stmt => {
                // DDL commits any open transaction first, then changes the committed state directly.
                self.commit()?;
                self.ddl(stmt, binds)?;
                Ok(QueryResult::default())
            }
        }
    }

    pub fn commit(&mut self) -> Result<(), OraError> {
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
                Change::Update { table, id, values } => {
                    if let Some(t) = next
                        .table_mut(&table)
                        .filter(|t| t.columns.len() == values.len())
                    {
                        t.update(vec![(id, values)])
                            .map_err(|c| unique_violation(&self.env.user, &c))?;
                    }
                }
                Change::Delete { table, id } => {
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

    pub fn rollback(&mut self) {
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

    fn dml(&mut self, stmt: &Statement, binds: &[Value]) -> Result<u64, OraError> {
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
        let saved = (txn.work.clone(), txn.log.len());
        let result = match stmt {
            Statement::Insert(i) => insert(&db, &self.env, txn, i, binds),
            Statement::Update(u) => update(&self.env, txn, u, binds),
            Statement::Delete(d) => delete(&self.env, txn, d, binds),
            _ => unreachable!(),
        };
        if result.is_err() {
            txn.work = saved.0;
            txn.log.truncate(saved.1);
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
            _ => unreachable!("not DDL"),
        }
        Ok(())
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
fn store_value(
    env: &Env,
    user: &str,
    table: &str,
    col: &TableColumn,
    v: Value,
) -> Result<Value, OraError> {
    if v.is_null() {
        return Ok(Value::Null);
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
                Value::Null => unreachable!(),
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

fn insert(
    db: &Database,
    env: &Env,
    txn: &mut Txn,
    ins: &Insert,
    binds: &[Value],
) -> Result<u64, OraError> {
    let ex = Ex {
        cat: &txn.work,
        binds,
        env,
    };
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
    Ok(count)
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

fn update(env: &Env, txn: &mut Txn, upd: &Update, binds: &[Value]) -> Result<u64, OraError> {
    let ex = Ex {
        cat: &txn.work,
        binds,
        env,
    };
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
    for (id, old) in matching_rows(&ex, t, &cols, &upd.where_)? {
        let mut new = old.clone();
        for ((_, e), &i) in upd.assignments.iter().zip(&targets) {
            let v = ex.eval(e, &Scope::row(&cols, &old, None))?;
            new[i] = store_value(env, &env.user, &t.name, &t.columns[i], v)?;
        }
        check_row(&ex, t, &new, true)?;
        changes.push((id, new));
    }
    let count = changes.len() as u64;
    let table = txn.work.table_mut(&upd.table).unwrap();
    table
        .update(changes.clone())
        .map_err(|c| unique_violation(&env.user, &c))?;
    for (id, values) in changes {
        txn.log.push(Change::Update {
            table: upd.table.clone(),
            id,
            values,
        });
    }
    Ok(count)
}

fn delete(env: &Env, txn: &mut Txn, del: &Delete, binds: &[Value]) -> Result<u64, OraError> {
    let ex = Ex {
        cat: &txn.work,
        binds,
        env,
    };
    let t = txn
        .work
        .table(&del.table)
        .ok_or_else(OraError::table_not_found)?;
    let cols = table_scope_cols(t, del.alias.as_deref().unwrap_or(&del.table));
    let ids: Vec<u64> = matching_rows(&ex, t, &cols, &del.where_)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let table = txn.work.table_mut(&del.table).unwrap();
    for &id in &ids {
        table.delete(id);
        txn.log.push(Change::Delete {
            table: del.table.clone(),
            id,
        });
    }
    Ok(ids.len() as u64)
}
