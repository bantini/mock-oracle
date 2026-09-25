//! Query and expression evaluation over a [`Catalog`].

use std::cmp::Ordering;
use std::collections::HashMap;

use bigdecimal::BigDecimal;
use chrono::{Datelike, Duration, NaiveDateTime};

use crate::ast::*;
use crate::catalog::Catalog;
use crate::value::{compare, key_string};
use crate::{datetime, OraError, SqlType, Value};

/// Per-session settings that affect evaluation.
#[derive(Debug, Clone)]
pub struct Env {
    pub user: String,
    pub nls_date_format: String,
    pub nls_timestamp_format: String,
    /// Session time zone as minutes east of UTC.
    pub tz_offset_minutes: i32,
    /// When the current statement started, in UTC and in the server's local time.
    pub now_utc: NaiveDateTime,
    pub now_local: NaiveDateTime,
}

impl Env {
    pub fn new(user: &str) -> Self {
        let now = chrono::Local::now();
        Self {
            user: user.to_uppercase(),
            nls_date_format: datetime::DEFAULT_DATE_FORMAT.into(),
            nls_timestamp_format: datetime::DEFAULT_TIMESTAMP_FORMAT.into(),
            tz_offset_minutes: now.offset().local_minus_utc() / 60,
            now_utc: now.naive_utc(),
            now_local: now.naive_local(),
        }
    }

    pub fn start_statement(&mut self) {
        let now = chrono::Local::now();
        self.now_utc = now.naive_utc();
        self.now_local = now.naive_local();
    }

    pub fn session_now(&self) -> NaiveDateTime {
        self.now_utc + Duration::minutes(self.tz_offset_minutes as i64)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RelCol {
    /// The table name or alias the column can be qualified with.
    pub qualifier: Option<String>,
    pub name: String,
    pub sql_type: SqlType,
}

#[derive(Debug, Clone, Default)]
pub struct Relation {
    pub cols: Vec<RelCol>,
    pub rows: Vec<Vec<Value>>,
}

/// What an expression can see: the current row, the rows of its group when aggregating,
/// and the enclosing query's scope for correlated subqueries.
#[derive(Clone, Copy)]
pub struct Scope<'a> {
    pub cols: &'a [RelCol],
    pub row: &'a [Value],
    pub group: Option<&'a [Vec<Value>]>,
    pub rownum: i64,
    pub parent: Option<&'a Scope<'a>>,
}

impl<'a> Scope<'a> {
    pub fn empty(parent: Option<&'a Scope<'a>>) -> Self {
        Scope {
            cols: &[],
            row: &[],
            group: None,
            rownum: 0,
            parent,
        }
    }

    pub fn row(cols: &'a [RelCol], row: &'a [Value], parent: Option<&'a Scope<'a>>) -> Self {
        Scope {
            cols,
            row,
            group: None,
            rownum: 0,
            parent,
        }
    }
}

pub struct Ex<'a> {
    pub cat: &'a Catalog,
    pub binds: &'a [Value],
    pub env: &'a Env,
}

const AGGREGATES: &[&str] = &[
    "COUNT", "SUM", "AVG", "MIN", "MAX", "STDDEV", "VARIANCE", "MEDIAN",
];

pub fn is_aggregate(name: &str) -> bool {
    AGGREGATES.contains(&name)
}

/// Whether an expression contains an aggregate call outside any subquery.
pub fn contains_aggregate(e: &Expr) -> bool {
    match e {
        Expr::CountStar => true,
        Expr::Function { name, args, .. } => {
            is_aggregate(name) || args.iter().any(contains_aggregate)
        }
        Expr::Literal(_) | Expr::Bind(_) | Expr::Column { .. } | Expr::RowNum => false,
        Expr::Subquery(_) | Expr::Exists(_) => false,
        Expr::Neg(a) | Expr::Not(a) => contains_aggregate(a),
        Expr::Binary(_, a, b) => contains_aggregate(a) || contains_aggregate(b),
        Expr::IsNull { expr, .. } | Expr::InSubquery { expr, .. } => contains_aggregate(expr),
        Expr::Between {
            expr, low, high, ..
        } => contains_aggregate(expr) || contains_aggregate(low) || contains_aggregate(high),
        Expr::InList { expr, list, .. } => {
            contains_aggregate(expr) || list.iter().any(contains_aggregate)
        }
        Expr::Like {
            expr,
            pattern,
            escape,
            ..
        } => {
            contains_aggregate(expr)
                || contains_aggregate(pattern)
                || escape.as_deref().is_some_and(contains_aggregate)
        }
        Expr::Case {
            operand,
            whens,
            else_,
        } => {
            operand.as_deref().is_some_and(contains_aggregate)
                || whens
                    .iter()
                    .any(|(w, t)| contains_aggregate(w) || contains_aggregate(t))
                || else_.as_deref().is_some_and(contains_aggregate)
        }
    }
}

fn invalid_column(table: Option<&str>, name: &str) -> OraError {
    match table {
        Some(t) => OraError::invalid_identifier(&format!("\"{t}\".\"{name}\"")),
        None => OraError::invalid_identifier(&format!("\"{name}\"")),
    }
}

/// Finds a column in `cols`. `Ok(None)` means not found here; two matches is ORA-00918.
pub fn find_column(
    cols: &[RelCol],
    table: Option<&str>,
    name: &str,
) -> Result<Option<usize>, OraError> {
    let mut found = None;
    for (i, c) in cols.iter().enumerate() {
        if c.name == name && table.map_or(true, |t| c.qualifier.as_deref() == Some(t)) {
            if found.is_some() {
                return Err(OraError::new(918, "column ambiguously defined"));
            }
            found = Some(i);
        }
    }
    Ok(found)
}

/// Rounds to the 40 significant digits an Oracle NUMBER holds.
pub fn round_number(n: BigDecimal) -> BigDecimal {
    if n.digits() > 40 {
        n.with_prec(40).normalized()
    } else {
        n
    }
}

fn number(n: BigDecimal) -> Value {
    Value::Number(n)
}

/// Kinds of ORDER BY item.
enum SortKey<'e> {
    Output(usize),
    Expr(&'e Expr),
}

/// A select-list item after `*` expansion.
enum Item<'e> {
    Source(usize),
    Expr(&'e Expr),
}

type KeyedRows = Vec<(Vec<Value>, Vec<Value>)>;

impl Ex<'_> {
    // ---- queries ----

    pub fn query(&self, q: &Query, parent: Option<&Scope>) -> Result<Relation, OraError> {
        let (cols, mut rows): (Vec<RelCol>, KeyedRows) = match &q.body {
            SetExpr::Select(s) => self.select(s, &q.order_by, parent)?,
            body => {
                let rel = self.set_expr(body, parent)?;
                let mut keys = Vec::new();
                for item in &q.order_by {
                    keys.push(match output_position(&item.expr, &rel.cols)? {
                        Some(i) => i,
                        None => {
                            return Err(OraError::new(
                                1785,
                                "ORDER BY item must be the number of a SELECT-list expression",
                            ))
                        }
                    });
                }
                let rows = rel
                    .rows
                    .into_iter()
                    .map(|r| (keys.iter().map(|&k| r[k].clone()).collect(), r))
                    .map(|(k, r)| (r, k))
                    .collect();
                (rel.cols, rows)
            }
        };
        if !q.order_by.is_empty() {
            self.sort(&mut rows, &q.order_by)?;
        }
        let mut rows: Vec<Vec<Value>> = rows.into_iter().map(|(r, _)| r).collect();
        let limit = |e: &Option<Expr>| -> Result<Option<usize>, OraError> {
            let Some(e) = e else { return Ok(None) };
            let v = self.eval(e, &Scope::empty(parent))?.to_number()?;
            Ok(Some(
                v.map(|n| n.with_scale_round(0, bigdecimal::RoundingMode::Down))
                    .map_or(0, |n| {
                        if n <= 0 {
                            0
                        } else {
                            n.to_string().parse::<usize>().unwrap_or(usize::MAX)
                        }
                    }),
            ))
        };
        if let Some(offset) = limit(&q.offset)? {
            rows.drain(..offset.min(rows.len()));
        }
        if let Some(fetch) = limit(&q.fetch)? {
            rows.truncate(fetch);
        }
        Ok(Relation { cols, rows })
    }

    fn sort(&self, rows: &mut KeyedRows, order_by: &[OrderItem]) -> Result<(), OraError> {
        let mut error = None;
        let fmt = &self.env.nls_date_format;
        rows.sort_by(|(_, a), (_, b)| {
            for (i, item) in order_by.iter().enumerate() {
                let nulls_first = item.nulls_first.unwrap_or(item.desc);
                let ord = match (a[i].is_null(), b[i].is_null()) {
                    (true, true) => Ordering::Equal,
                    (true, false) => {
                        return if nulls_first {
                            Ordering::Less
                        } else {
                            Ordering::Greater
                        }
                    }
                    (false, true) => {
                        return if nulls_first {
                            Ordering::Greater
                        } else {
                            Ordering::Less
                        }
                    }
                    _ => match compare(&a[i], &b[i], fmt) {
                        Ok(o) => o.unwrap_or(Ordering::Equal),
                        Err(e) => {
                            error.get_or_insert(e);
                            Ordering::Equal
                        }
                    },
                };
                let ord = if item.desc { ord.reverse() } else { ord };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
            Ordering::Equal
        });
        error.map_or(Ok(()), Err)
    }

    fn set_expr(&self, body: &SetExpr, parent: Option<&Scope>) -> Result<Relation, OraError> {
        match body {
            SetExpr::Select(s) => {
                let (cols, rows) = self.select(s, &[], parent)?;
                Ok(Relation {
                    cols,
                    rows: rows.into_iter().map(|(r, _)| r).collect(),
                })
            }
            SetExpr::SetOp { op, left, right } => {
                let mut l = self.set_expr(left, parent)?;
                let r = self.set_expr(right, parent)?;
                if l.cols.len() != r.cols.len() {
                    return Err(OraError::new(
                        1789,
                        "query block has incorrect number of result columns",
                    ));
                }
                for (a, b) in l.cols.iter_mut().zip(&r.cols) {
                    a.sql_type = union_type(a.sql_type, b.sql_type)?;
                }
                let key = |row: &Vec<Value>| key_string(row);
                let rows = match op {
                    SetOp::UnionAll => {
                        l.rows.extend(r.rows);
                        l.rows
                    }
                    SetOp::Union => {
                        l.rows.extend(r.rows);
                        distinct(l.rows)
                    }
                    SetOp::Intersect => {
                        let right: std::collections::HashSet<String> =
                            r.rows.iter().map(key).collect();
                        distinct(
                            l.rows
                                .into_iter()
                                .filter(|row| right.contains(&key(row)))
                                .collect(),
                        )
                    }
                    SetOp::Minus => {
                        let right: std::collections::HashSet<String> =
                            r.rows.iter().map(key).collect();
                        distinct(
                            l.rows
                                .into_iter()
                                .filter(|row| !right.contains(&key(row)))
                                .collect(),
                        )
                    }
                };
                let cols = l.cols;
                let rows = rows
                    .into_iter()
                    .map(|row| coerce_row(row, &cols, self.env))
                    .collect::<Result<_, _>>()?;
                Ok(Relation { cols, rows })
            }
        }
    }

    /// Runs one SELECT block. Each result row comes with its ORDER BY key values.
    fn select(
        &self,
        s: &Select,
        order_by: &[OrderItem],
        parent: Option<&Scope>,
    ) -> Result<(Vec<RelCol>, KeyedRows), OraError> {
        // FROM
        let mut source: Option<Relation> = None;
        for item in &s.from {
            let rel = self.source_item(item, parent)?;
            source = Some(match source {
                None => rel,
                Some(left) => cross(left, rel),
            });
        }
        let source = source.unwrap_or_default();

        // Select list
        let mut items: Vec<(Item, String)> = Vec::new();
        for item in &s.items {
            match item {
                SelectItem::Star => items.extend(
                    source
                        .cols
                        .iter()
                        .enumerate()
                        .map(|(i, c)| (Item::Source(i), c.name.clone())),
                ),
                SelectItem::QualifiedStar(t) => {
                    let before = items.len();
                    items.extend(
                        source
                            .cols
                            .iter()
                            .enumerate()
                            .filter(|(_, c)| c.qualifier.as_deref() == Some(t))
                            .map(|(i, c)| (Item::Source(i), c.name.clone())),
                    );
                    if items.len() == before {
                        return Err(OraError::invalid_identifier(&format!("\"{t}\"")));
                    }
                }
                SelectItem::Expr { expr, name } => items.push((Item::Expr(expr), name.clone())),
            }
        }
        let names: Vec<&str> = items.iter().map(|(_, n)| n.as_str()).collect();

        // ORDER BY keys: a position, a select-list alias, or an expression over the source.
        let mut keys = Vec::new();
        for o in order_by {
            let by_name = match &o.expr {
                Expr::Column { table: None, name } => {
                    let matches: Vec<usize> = names
                        .iter()
                        .enumerate()
                        .filter(|(_, n)| *n == name)
                        .map(|(i, _)| i)
                        .collect();
                    match matches.len() {
                        0 => None,
                        1 => Some(matches[0]),
                        // The same column selected twice is fine; different expressions with one alias are not.
                        _ => {
                            if matches.iter().all(|&i| {
                                matches!(
                                    items[i].0,
                                    Item::Expr(Expr::Column { .. }) | Item::Source(_)
                                )
                            }) {
                                Some(matches[0])
                            } else {
                                return Err(OraError::new(
                                    960,
                                    "ambiguous column naming in select list",
                                ));
                            }
                        }
                    }
                }
                _ => None,
            };
            keys.push(match (position_literal(&o.expr), by_name) {
                (Some(p), _) => {
                    if p < 1 || p as usize > items.len() {
                        return Err(OraError::new(
                            1785,
                            "ORDER BY item must be the number of a SELECT-list expression",
                        ));
                    }
                    SortKey::Output(p as usize - 1)
                }
                (None, Some(i)) => SortKey::Output(i),
                (None, None) => SortKey::Expr(&o.expr),
            });
        }

        // WHERE, numbering accepted rows for ROWNUM.
        let mut accepted: Vec<(i64, Vec<Value>)> = Vec::new();
        for row in source.rows {
            let rownum = accepted.len() as i64 + 1;
            let scope = Scope {
                cols: &source.cols,
                row: &row,
                group: None,
                rownum,
                parent,
            };
            if let Some(w) = &s.where_ {
                if contains_aggregate(w) {
                    return Err(OraError::new(934, "group function is not allowed here"));
                }
                if self.eval_bool(w, &scope)? != Some(true) {
                    continue;
                }
            }
            accepted.push((rownum, row));
        }

        let aggregate = !s.group_by.is_empty()
            || s.having.is_some()
            || items
                .iter()
                .any(|(i, _)| matches!(i, Item::Expr(e) if contains_aggregate(e)))
            || keys
                .iter()
                .any(|k| matches!(k, SortKey::Expr(e) if contains_aggregate(e)));

        let mut out: KeyedRows = Vec::new();
        let compute = |scope: &Scope, out: &mut KeyedRows| -> Result<(), OraError> {
            let mut values = Vec::with_capacity(items.len());
            for (item, _) in &items {
                values.push(match item {
                    Item::Source(i) => scope.row[*i].clone(),
                    Item::Expr(e) => self.eval(e, scope)?,
                });
            }
            let mut key_values = Vec::with_capacity(keys.len());
            for k in &keys {
                key_values.push(match k {
                    SortKey::Output(i) => values[*i].clone(),
                    SortKey::Expr(e) => self.eval(e, scope)?,
                });
            }
            out.push((values, key_values));
            Ok(())
        };

        if aggregate {
            for (item, _) in &items {
                if let Item::Source(_) = item {
                    return Err(OraError::new(979, "not a GROUP BY expression"));
                }
            }
            // Group rows by their GROUP BY values, keeping first-seen order.
            let mut groups: Vec<Vec<Vec<Value>>> = Vec::new();
            if s.group_by.is_empty() {
                groups.push(accepted.into_iter().map(|(_, r)| r).collect());
            } else {
                let mut index: HashMap<String, usize> = HashMap::new();
                for (rownum, row) in accepted {
                    let scope = Scope {
                        cols: &source.cols,
                        row: &row,
                        group: None,
                        rownum,
                        parent,
                    };
                    let mut key = Vec::with_capacity(s.group_by.len());
                    for g in &s.group_by {
                        if contains_aggregate(g) {
                            return Err(OraError::new(934, "group function is not allowed here"));
                        }
                        key.push(self.eval(g, &scope)?);
                    }
                    let k = key_string(&key);
                    let slot = *index.entry(k).or_insert_with(|| {
                        groups.push(Vec::new());
                        groups.len() - 1
                    });
                    groups[slot].push(row);
                }
            }
            let null_row = vec![Value::Null; source.cols.len()];
            for (n, group) in groups.iter().enumerate() {
                let first = group.first().unwrap_or(&null_row);
                let scope = Scope {
                    cols: &source.cols,
                    row: first,
                    group: Some(group),
                    rownum: n as i64 + 1,
                    parent,
                };
                if let Some(h) = &s.having {
                    if self.eval_bool(h, &scope)? != Some(true) {
                        continue;
                    }
                }
                compute(&scope, &mut out)?;
            }
        } else {
            for (rownum, row) in &accepted {
                let scope = Scope {
                    cols: &source.cols,
                    row,
                    group: None,
                    rownum: *rownum,
                    parent,
                };
                compute(&scope, &mut out)?;
            }
        }

        if s.distinct {
            let mut seen = std::collections::HashSet::new();
            out.retain(|(values, _)| seen.insert(key_string(values)));
        }

        // Result column types: declared where the expression has one, otherwise from the values.
        let mut cols = Vec::with_capacity(items.len());
        for (i, (item, name)) in items.iter().enumerate() {
            let declared = match item {
                Item::Source(c) => Some(source.cols[*c].sql_type),
                Item::Expr(e) => self.static_type(e, &source.cols),
            };
            let sql_type = infer_type(declared, out.iter().map(|(r, _)| &r[i]));
            cols.push(RelCol {
                qualifier: None,
                name: name.clone(),
                sql_type,
            });
        }
        for (values, _) in &mut out {
            let row = std::mem::take(values);
            *values = coerce_row(row, &cols, self.env)?;
        }
        Ok((cols, out))
    }

    fn source_item(&self, item: &FromItem, parent: Option<&Scope>) -> Result<Relation, OraError> {
        let mut left = self.table_factor(&item.factor, parent)?;
        for join in &item.joins {
            let right = self.table_factor(&join.factor, parent)?;
            left = match join.kind {
                JoinKind::Cross => cross(left, right),
                kind => self.join(kind, left, right, join.on.as_ref(), parent)?,
            };
        }
        Ok(left)
    }

    fn join(
        &self,
        kind: JoinKind,
        left: Relation,
        right: Relation,
        on: Option<&Expr>,
        parent: Option<&Scope>,
    ) -> Result<Relation, OraError> {
        let mut cols = left.cols.clone();
        cols.extend(right.cols.iter().cloned());
        let (lw, rw) = (left.cols.len(), right.cols.len());
        let mut rows = Vec::new();
        let mut right_matched = vec![false; right.rows.len()];
        for l in &left.rows {
            let mut matched = false;
            for (ri, r) in right.rows.iter().enumerate() {
                let mut row = l.clone();
                row.extend(r.iter().cloned());
                let ok = match on {
                    Some(on) => self.eval_bool(on, &Scope::row(&cols, &row, parent))? == Some(true),
                    None => true,
                };
                if ok {
                    matched = true;
                    right_matched[ri] = true;
                    rows.push(row);
                }
            }
            if !matched && matches!(kind, JoinKind::Left | JoinKind::Full) {
                let mut row = l.clone();
                row.extend(std::iter::repeat(Value::Null).take(rw));
                rows.push(row);
            }
        }
        if matches!(kind, JoinKind::Right | JoinKind::Full) {
            for (r, matched) in right.rows.iter().zip(right_matched) {
                if !matched {
                    let mut row = vec![Value::Null; lw];
                    row.extend(r.iter().cloned());
                    rows.push(row);
                }
            }
        }
        Ok(Relation { cols, rows })
    }

    fn table_factor(
        &self,
        factor: &TableFactor,
        parent: Option<&Scope>,
    ) -> Result<Relation, OraError> {
        match factor {
            TableFactor::Table { name, alias } => {
                let qualifier = Some(alias.clone().unwrap_or_else(|| name.clone()));
                match self.cat.table(name) {
                    Some(t) => Ok(Relation {
                        cols: t
                            .columns
                            .iter()
                            .map(|c| RelCol {
                                qualifier: qualifier.clone(),
                                name: c.name.clone(),
                                sql_type: c.sql_type,
                            })
                            .collect(),
                        rows: t.rows.values().cloned().collect(),
                    }),
                    None if name == "DUAL" => Ok(Relation {
                        cols: vec![RelCol {
                            qualifier,
                            name: "DUMMY".into(),
                            sql_type: SqlType::Varchar2(1),
                        }],
                        rows: vec![vec![Value::varchar("X")]],
                    }),
                    None => Err(OraError::table_not_found()),
                }
            }
            TableFactor::Subquery { query, alias } => {
                let mut rel = self.query(query, parent)?;
                for c in &mut rel.cols {
                    c.qualifier = alias.clone();
                }
                Ok(rel)
            }
        }
    }

    /// The type an expression has regardless of the data, when that is known.
    pub fn static_type(&self, e: &Expr, cols: &[RelCol]) -> Option<SqlType> {
        match e {
            Expr::Column { table, name } => find_column(cols, table.as_deref(), name)
                .ok()
                .flatten()
                .map(|i| cols[i].sql_type),
            Expr::Literal(Value::Number(_)) | Expr::CountStar | Expr::RowNum => {
                Some(SqlType::NUMBER)
            }
            Expr::Literal(Value::Date(_)) => Some(SqlType::Date),
            Expr::Literal(Value::Timestamp(_)) => Some(SqlType::Timestamp(9)),
            Expr::Neg(_) => Some(SqlType::NUMBER),
            Expr::Binary(op, a, b) => match op {
                BinaryOp::Mul | BinaryOp::Div => Some(SqlType::NUMBER),
                BinaryOp::Add | BinaryOp::Sub => {
                    let (ta, tb) = (self.static_type(a, cols), self.static_type(b, cols));
                    let is_date = |t: Option<SqlType>| {
                        matches!(t, Some(SqlType::Date) | Some(SqlType::Timestamp(_)))
                    };
                    match (is_date(ta), is_date(tb)) {
                        (true, true) => Some(SqlType::NUMBER),
                        (true, false) | (false, true) => Some(SqlType::Date),
                        _ => Some(SqlType::NUMBER),
                    }
                }
                _ => None,
            },
            Expr::Function { name, args, .. } => {
                let first = || args.first().and_then(|a| self.static_type(a, cols));
                match name.as_str() {
                    "COUNT" | "SUM" | "AVG" | "STDDEV" | "VARIANCE" | "MEDIAN" | "LENGTH"
                    | "LENGTHB" | "INSTR" | "ABS" | "SIGN" | "CEIL" | "FLOOR" | "MOD" | "POWER"
                    | "SQRT" | "TO_NUMBER" | "ASCII" | "MONTHS_BETWEEN" | "EXP" | "LN" | "LOG"
                    | "REMAINDER" => Some(SqlType::NUMBER),
                    "SYSDATE" | "CURRENT_DATE" | "TO_DATE" | "ADD_MONTHS" | "LAST_DAY"
                    | "NEXT_DAY" => Some(SqlType::Date),
                    "SYSTIMESTAMP" | "CURRENT_TIMESTAMP" | "LOCALTIMESTAMP" | "TO_TIMESTAMP" => {
                        Some(SqlType::Timestamp(6))
                    }
                    "ROUND" | "TRUNC" => match first() {
                        Some(SqlType::Date) | Some(SqlType::Timestamp(_)) => Some(SqlType::Date),
                        _ => Some(SqlType::NUMBER),
                    },
                    "MIN" | "MAX" | "NVL" | "COALESCE" | "GREATEST" | "LEAST" => first(),
                    "UPPER" | "LOWER" | "INITCAP" => match first() {
                        Some(t @ SqlType::Varchar2(_)) | Some(t @ SqlType::Char(_)) => Some(t),
                        _ => None,
                    },
                    "EXTRACT" => Some(SqlType::NUMBER),
                    "CAST" => match &args[1] {
                        Expr::Literal(Value::Varchar2(t)) => match t.as_str() {
                            "NUMBER" => Some(SqlType::NUMBER),
                            "DATE" => Some(SqlType::Date),
                            "TIMESTAMP" => Some(SqlType::Timestamp(6)),
                            t => t
                                .strip_prefix("VARCHAR2:")
                                .and_then(|n| n.parse().ok())
                                .map(SqlType::Varchar2),
                        },
                        _ => None,
                    },
                    _ => None,
                }
            }
            Expr::Case { whens, else_, .. } => whens
                .iter()
                .map(|(_, t)| t)
                .chain(else_.as_deref())
                .find_map(|t| self.static_type(t, cols))
                .filter(|t| {
                    matches!(
                        t,
                        SqlType::Number { .. } | SqlType::Date | SqlType::Timestamp(_)
                    )
                }),
            _ => None,
        }
    }

    /// Runs a query that must return one column, for IN and scalar subqueries.
    fn single_column(&self, q: &Query, scope: &Scope) -> Result<Vec<Value>, OraError> {
        let rel = self.query(q, Some(scope))?;
        if rel.cols.len() != 1 {
            return Err(OraError::new(913, "too many values"));
        }
        Ok(rel.rows.into_iter().map(|mut r| r.pop().unwrap()).collect())
    }

    // ---- expressions ----

    fn resolve(&self, scope: &Scope, table: Option<&str>, name: &str) -> Result<Value, OraError> {
        let mut s = Some(scope);
        while let Some(sc) = s {
            if let Some(i) = find_column(sc.cols, table, name)? {
                return Ok(sc.row.get(i).cloned().unwrap_or(Value::Null));
            }
            s = sc.parent;
        }
        Err(invalid_column(table, name))
    }

    pub fn eval(&self, e: &Expr, scope: &Scope) -> Result<Value, OraError> {
        match e {
            Expr::Literal(v) => Ok(v.clone()),
            Expr::Bind(i) => Ok(self.binds.get(*i).cloned().unwrap_or(Value::Null)),
            Expr::Column { table, name } => self.resolve(scope, table.as_deref(), name),
            Expr::RowNum => Ok(Value::number(scope.rownum)),
            Expr::Neg(a) => Ok(match self.eval(a, scope)?.to_number()? {
                Some(n) => number(-n),
                None => Value::Null,
            }),
            Expr::Binary(op, a, b) => match op {
                BinaryOp::And
                | BinaryOp::Or
                | BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq => Err(OraError::new(920, "invalid relational operator")),
                BinaryOp::Concat => {
                    let (x, y) = (self.eval(a, scope)?, self.eval(b, scope)?);
                    let mut s = self.to_text(&x).unwrap_or_default();
                    s.push_str(&self.to_text(&y).unwrap_or_default());
                    Ok(Value::varchar(s))
                }
                op => {
                    let (x, y) = (self.eval(a, scope)?, self.eval(b, scope)?);
                    self.arithmetic(*op, x, y)
                }
            },
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o, scope)?),
                    None => None,
                };
                for (when, then) in whens {
                    let hit = match &subject {
                        Some(s) => {
                            let w = self.eval(when, scope)?;
                            compare(s, &w, &self.env.nls_date_format)? == Some(Ordering::Equal)
                        }
                        None => self.eval_bool(when, scope)? == Some(true),
                    };
                    if hit {
                        return self.eval(then, scope);
                    }
                }
                match else_ {
                    Some(e) => self.eval(e, scope),
                    None => Ok(Value::Null),
                }
            }
            Expr::CountStar => {
                let group = scope
                    .group
                    .ok_or_else(|| OraError::new(934, "group function is not allowed here"))?;
                Ok(Value::number(group.len() as i64))
            }
            Expr::Function {
                name,
                args,
                distinct,
            } if is_aggregate(name) => self.aggregate(name, args, *distinct, scope),
            Expr::Function { name, args, .. } => {
                let mut values = Vec::with_capacity(args.len());
                for a in args {
                    values.push(self.eval(a, scope)?);
                }
                self.call(name, values)
            }
            Expr::Subquery(q) => {
                let mut values = self.single_column(q, scope)?;
                match values.len() {
                    0 => Ok(Value::Null),
                    1 => Ok(values.pop().unwrap()),
                    _ => Err(OraError::new(
                        1427,
                        "single-row subquery returns more than one row",
                    )),
                }
            }
            Expr::Not(_)
            | Expr::IsNull { .. }
            | Expr::Between { .. }
            | Expr::InList { .. }
            | Expr::InSubquery { .. }
            | Expr::Exists(_)
            | Expr::Like { .. } => Err(OraError::new(920, "invalid relational operator")),
        }
    }

    fn aggregate(
        &self,
        name: &str,
        args: &[Expr],
        distinct: bool,
        scope: &Scope,
    ) -> Result<Value, OraError> {
        let group = scope
            .group
            .ok_or_else(|| OraError::new(934, "group function is not allowed here"))?;
        if args.len() != 1 {
            return Err(OraError::new(909, "invalid number of arguments"));
        }
        let mut values = Vec::with_capacity(group.len());
        for row in group {
            let inner = Scope {
                row,
                group: None,
                ..*scope
            };
            let v = self.eval(&args[0], &inner)?;
            if !v.is_null() {
                values.push(v);
            }
        }
        if distinct {
            let mut seen = std::collections::HashSet::new();
            values.retain(|v| seen.insert(key_string([v])));
        }
        let numbers = |values: &[Value]| -> Result<Vec<BigDecimal>, OraError> {
            values
                .iter()
                .map(|v| v.to_number().map(|n| n.unwrap()))
                .collect()
        };
        let fmt = &self.env.nls_date_format;
        match name {
            "COUNT" => Ok(Value::number(values.len() as i64)),
            "SUM" if values.is_empty() => Ok(Value::Null),
            "SUM" => Ok(number(numbers(&values)?.into_iter().sum())),
            "AVG" if values.is_empty() => Ok(Value::Null),
            "AVG" => {
                let n = values.len() as i64;
                let sum: BigDecimal = numbers(&values)?.into_iter().sum();
                Ok(number(round_number(sum / BigDecimal::from(n))))
            }
            "MIN" | "MAX" => {
                let mut best: Option<Value> = None;
                for v in values {
                    best = Some(match best {
                        None => v,
                        Some(b) => {
                            let ord = compare(&v, &b, fmt)?.unwrap_or(Ordering::Equal);
                            if (name == "MIN") == (ord == Ordering::Less) && ord != Ordering::Equal
                            {
                                v
                            } else {
                                b
                            }
                        }
                    });
                }
                Ok(best.unwrap_or(Value::Null))
            }
            "MEDIAN" => {
                let mut ns = numbers(&values)?;
                if ns.is_empty() {
                    return Ok(Value::Null);
                }
                ns.sort();
                let mid = ns.len() / 2;
                Ok(number(if ns.len() % 2 == 1 {
                    ns[mid].clone()
                } else {
                    round_number((&ns[mid - 1] + &ns[mid]) / BigDecimal::from(2))
                }))
            }
            _ => {
                // STDDEV and VARIANCE (sample), 0 for a single value as Oracle does.
                let ns = numbers(&values)?;
                if ns.is_empty() {
                    return Ok(Value::Null);
                }
                if ns.len() == 1 {
                    return Ok(Value::number(0));
                }
                let count = BigDecimal::from(ns.len() as i64);
                let mean = ns.iter().sum::<BigDecimal>() / &count;
                let var = round_number(
                    ns.iter()
                        .map(|x| (x - &mean) * (x - &mean))
                        .sum::<BigDecimal>()
                        / (count - BigDecimal::from(1)),
                );
                if name == "VARIANCE" {
                    Ok(number(var))
                } else {
                    Ok(number(round_number(var.sqrt().unwrap_or_default())))
                }
            }
        }
    }

    fn arithmetic(&self, op: BinaryOp, x: Value, y: Value) -> Result<Value, OraError> {
        if x.is_null() || y.is_null() {
            return Ok(Value::Null);
        }
        let is_date = |v: &Value| matches!(v, Value::Date(_) | Value::Timestamp(_));
        match (op, is_date(&x), is_date(&y)) {
            (BinaryOp::Sub, true, true) => {
                let (a, b) = (x.to_datetime("")?.unwrap(), y.to_datetime("")?.unwrap());
                let nanos = (a - b).num_nanoseconds().unwrap_or(0);
                let days = BigDecimal::from(nanos) / BigDecimal::from(86_400_000_000_000i64);
                Ok(number(round_number(days)))
            }
            (BinaryOp::Add, true, false)
            | (BinaryOp::Sub, true, false)
            | (BinaryOp::Add, false, true) => {
                let (d, n) = if is_date(&x) { (x, y) } else { (y, x) };
                let d = d.to_datetime("")?.unwrap();
                let days = n.to_number()?.unwrap();
                let secs = (days * BigDecimal::from(86_400))
                    .with_scale_round(0, bigdecimal::RoundingMode::HalfUp);
                let secs: i64 = secs
                    .to_string()
                    .parse()
                    .map_err(|_| datetime::out_of_range())?;
                let secs = if op == BinaryOp::Sub { -secs } else { secs };
                let d = Duration::try_seconds(secs)
                    .and_then(|delta| d.with_nanosecond_zero().checked_add_signed(delta))
                    .filter(|d| (-4713..=9999).contains(&d.year()))
                    .ok_or_else(datetime::out_of_range)?;
                Ok(Value::Date(d))
            }
            (_, true, _) | (_, _, true) => Err(OraError::inconsistent("NUMBER", "DATE")),
            _ => {
                let (a, b) = (x.to_number()?.unwrap(), y.to_number()?.unwrap());
                Ok(number(match op {
                    BinaryOp::Add => a + b,
                    BinaryOp::Sub => a - b,
                    BinaryOp::Mul => round_number(a * b),
                    BinaryOp::Div => {
                        if b == 0 {
                            return Err(OraError::new(1476, "divisor is equal to zero"));
                        }
                        round_number(a / b)
                    }
                    _ => unreachable!(),
                }))
            }
        }
    }

    /// Implicit conversion to VARCHAR2, using the session's date formats.
    pub fn to_text(&self, v: &Value) -> Option<String> {
        match v {
            Value::Null => None,
            Value::Date(d) => Some(datetime::format(d, &self.env.nls_date_format)),
            Value::Timestamp(d) => Some(datetime::format(d, &self.env.nls_timestamp_format)),
            v => Some(v.to_string()),
        }
    }

    // ---- conditions ----

    /// Evaluates a condition with SQL's three-valued logic: `None` is UNKNOWN.
    pub fn eval_bool(&self, e: &Expr, scope: &Scope) -> Result<Option<bool>, OraError> {
        let fmt = &self.env.nls_date_format;
        match e {
            Expr::Binary(BinaryOp::And, a, b) => {
                let x = self.eval_bool(a, scope)?;
                if x == Some(false) {
                    return Ok(Some(false));
                }
                Ok(match (x, self.eval_bool(b, scope)?) {
                    (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                })
            }
            Expr::Binary(BinaryOp::Or, a, b) => {
                let x = self.eval_bool(a, scope)?;
                if x == Some(true) {
                    return Ok(Some(true));
                }
                Ok(match (x, self.eval_bool(b, scope)?) {
                    (_, Some(true)) => Some(true),
                    (Some(false), Some(false)) => Some(false),
                    _ => None,
                })
            }
            Expr::Binary(
                op @ (BinaryOp::Eq
                | BinaryOp::NotEq
                | BinaryOp::Lt
                | BinaryOp::LtEq
                | BinaryOp::Gt
                | BinaryOp::GtEq),
                a,
                b,
            ) => {
                let (x, y) = (self.eval(a, scope)?, self.eval(b, scope)?);
                Ok(compare(&x, &y, fmt)?.map(|o| match op {
                    BinaryOp::Eq => o == Ordering::Equal,
                    BinaryOp::NotEq => o != Ordering::Equal,
                    BinaryOp::Lt => o == Ordering::Less,
                    BinaryOp::LtEq => o != Ordering::Greater,
                    BinaryOp::Gt => o == Ordering::Greater,
                    _ => o != Ordering::Less,
                }))
            }
            Expr::Not(a) => Ok(self.eval_bool(a, scope)?.map(|b| !b)),
            Expr::IsNull { expr, negated } => {
                Ok(Some(self.eval(expr, scope)?.is_null() != *negated))
            }
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => {
                let v = self.eval(expr, scope)?;
                let lo = compare(&v, &self.eval(low, scope)?, fmt)?.map(|o| o != Ordering::Less);
                let hi =
                    compare(&v, &self.eval(high, scope)?, fmt)?.map(|o| o != Ordering::Greater);
                let r = match (lo, hi) {
                    (Some(false), _) | (_, Some(false)) => Some(false),
                    (Some(true), Some(true)) => Some(true),
                    _ => None,
                };
                Ok(r.map(|b| b != *negated))
            }
            Expr::InList {
                expr,
                list,
                negated,
            } => {
                let v = self.eval(expr, scope)?;
                let mut candidates = Vec::with_capacity(list.len());
                for item in list {
                    candidates.push(self.eval(item, scope)?);
                }
                Ok(in_values(&v, &candidates, fmt)?.map(|b| b != *negated))
            }
            Expr::InSubquery {
                expr,
                query,
                negated,
            } => {
                let v = self.eval(expr, scope)?;
                let candidates = self.single_column(query, scope)?;
                Ok(in_values(&v, &candidates, fmt)?.map(|b| b != *negated))
            }
            Expr::Exists(q) => Ok(Some(!self.query(q, Some(scope))?.rows.is_empty())),
            Expr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => {
                let v = self.eval(expr, scope)?;
                let p = self.eval(pattern, scope)?;
                let esc = match escape {
                    Some(e) => match self.to_text(&self.eval(e, scope)?) {
                        Some(s) if s.chars().count() == 1 => Some(s.chars().next().unwrap()),
                        _ => {
                            return Err(OraError::new(
                                1425,
                                "escape character must be character string of length 1",
                            ))
                        }
                    },
                    None => None,
                };
                let (Some(s), Some(p)) = (self.to_text(&v), self.to_text(&p)) else {
                    return Ok(None);
                };
                Ok(Some(like(&s, &p, esc)? != *negated))
            }
            _ => Err(OraError::new(920, "invalid relational operator")),
        }
    }
}

trait ZeroNanos {
    fn with_nanosecond_zero(self) -> Self;
}

impl ZeroNanos for NaiveDateTime {
    fn with_nanosecond_zero(self) -> Self {
        use chrono::Timelike;
        self.with_nanosecond(0).unwrap_or(self)
    }
}

/// `v IN (candidates)` with SQL NULL semantics.
fn in_values(v: &Value, candidates: &[Value], fmt: &str) -> Result<Option<bool>, OraError> {
    if v.is_null() {
        return Ok(None);
    }
    let mut unknown = false;
    for c in candidates {
        match compare(v, c, fmt)? {
            Some(Ordering::Equal) => return Ok(Some(true)),
            None => unknown = true,
            _ => {}
        }
    }
    Ok(if unknown { None } else { Some(false) })
}

/// SQL LIKE with `%`, `_` and an optional escape character.
pub fn like(s: &str, pattern: &str, escape: Option<char>) -> Result<bool, OraError> {
    enum P {
        Any,
        One,
        Lit(char),
    }
    let mut parts = Vec::new();
    let mut chars = pattern.chars();
    while let Some(c) = chars.next() {
        if Some(c) == escape {
            match chars.next() {
                Some(n @ ('%' | '_')) => parts.push(P::Lit(n)),
                Some(n) if Some(n) == escape => parts.push(P::Lit(n)),
                _ => {
                    return Err(OraError::new(
                        1424,
                        "missing or illegal character following the escape character",
                    ))
                }
            }
        } else if c == '%' {
            parts.push(P::Any);
        } else if c == '_' {
            parts.push(P::One);
        } else {
            parts.push(P::Lit(c));
        }
    }
    let s: Vec<char> = s.chars().collect();
    // Iterative wildcard matching with backtracking to the last '%'.
    let (mut si, mut pi) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while si < s.len() {
        match parts.get(pi) {
            Some(P::One) => {
                si += 1;
                pi += 1;
            }
            Some(P::Lit(c)) if *c == s[si] => {
                si += 1;
                pi += 1;
            }
            Some(P::Any) => {
                star = Some((pi, si));
                pi += 1;
            }
            _ => match star {
                Some((sp, ss)) => {
                    pi = sp + 1;
                    si = ss + 1;
                    star = Some((sp, ss + 1));
                }
                None => return Ok(false),
            },
        }
    }
    Ok(parts[pi..].iter().all(|p| matches!(p, P::Any)))
}

fn cross(left: Relation, right: Relation) -> Relation {
    let mut cols = left.cols;
    cols.extend(right.cols);
    let mut rows = Vec::with_capacity(left.rows.len() * right.rows.len());
    for l in &left.rows {
        for r in &right.rows {
            let mut row = l.clone();
            row.extend(r.iter().cloned());
            rows.push(row);
        }
    }
    Relation { cols, rows }
}

fn distinct(rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    let mut seen = std::collections::HashSet::new();
    rows.into_iter()
        .filter(|r| seen.insert(key_string(r)))
        .collect()
}

fn position_literal(e: &Expr) -> Option<i64> {
    match e {
        Expr::Literal(Value::Number(n)) => n.to_string().parse().ok(),
        _ => None,
    }
}

/// An ORDER BY item of a set operation: a position or an output column name.
fn output_position(e: &Expr, cols: &[RelCol]) -> Result<Option<usize>, OraError> {
    if let Some(p) = position_literal(e) {
        if p < 1 || p as usize > cols.len() {
            return Err(OraError::new(
                1785,
                "ORDER BY item must be the number of a SELECT-list expression",
            ));
        }
        return Ok(Some(p as usize - 1));
    }
    match e {
        Expr::Column { table: None, name } => Ok(cols.iter().position(|c| &c.name == name)),
        _ => Ok(None),
    }
}

fn union_type(a: SqlType, b: SqlType) -> Result<SqlType, OraError> {
    use SqlType::*;
    Ok(match (a, b) {
        // An all-NULL column takes the other side's type.
        (Varchar2(0), t) | (t, Varchar2(0)) => t,
        (Number { .. }, Number { .. }) => SqlType::NUMBER,
        (Varchar2(x) | Char(x), Varchar2(y) | Char(y)) => Varchar2(x.max(y)),
        (Date, Date) => Date,
        (Date | Timestamp(_), Date | Timestamp(_)) => Timestamp(9),
        _ => {
            return Err(OraError::new(
                1790,
                "expression must have same datatype as corresponding expression",
            ))
        }
    })
}

/// The column type: the declared one if known, otherwise inferred from the values.
pub fn infer_type<'a>(
    declared: Option<SqlType>,
    values: impl Iterator<Item = &'a Value> + Clone,
) -> SqlType {
    if let Some(t) = declared {
        return t;
    }
    let mut max_len = 0u32;
    let mut kind = None;
    for v in values {
        match v {
            Value::Null => {}
            Value::Varchar2(s) => max_len = max_len.max(s.len() as u32),
            other => {
                kind.get_or_insert(match other {
                    Value::Number(_) => SqlType::NUMBER,
                    Value::Date(_) => SqlType::Date,
                    _ => SqlType::Timestamp(9),
                });
            }
        }
    }
    match kind {
        Some(t) if max_len == 0 => t,
        _ => SqlType::Varchar2(max_len),
    }
}

/// Converts a value to a result column's type, as Oracle does when a CASE or UNION mixes types.
pub fn coerce_row(row: Vec<Value>, cols: &[RelCol], env: &Env) -> Result<Vec<Value>, OraError> {
    row.into_iter()
        .zip(cols)
        .map(|(v, c)| {
            Ok(match (c.sql_type, v) {
                (_, Value::Null) => Value::Null,
                (SqlType::Number { .. }, v @ Value::Varchar2(_)) => {
                    Value::Number(v.to_number()?.unwrap())
                }
                (SqlType::Varchar2(_) | SqlType::Char(_), Value::Number(n)) => {
                    Value::varchar(crate::format_number(&n))
                }
                (SqlType::Varchar2(_) | SqlType::Char(_), Value::Date(d)) => {
                    Value::varchar(datetime::format(&d, &env.nls_date_format))
                }
                (SqlType::Varchar2(_) | SqlType::Char(_), Value::Timestamp(d)) => {
                    Value::varchar(datetime::format(&d, &env.nls_timestamp_format))
                }
                (SqlType::Date, Value::Timestamp(d)) => Value::Date(d.with_nanosecond_zero()),
                (SqlType::Timestamp(_), Value::Date(d)) => Value::Timestamp(d),
                (SqlType::Date | SqlType::Timestamp(_), Value::Number(_)) => {
                    return Err(OraError::inconsistent("DATE", "NUMBER"))
                }
                (SqlType::Number { .. }, Value::Date(_) | Value::Timestamp(_)) => {
                    return Err(OraError::inconsistent("NUMBER", "DATE"))
                }
                (SqlType::Date, v @ Value::Varchar2(_)) => {
                    Value::Date(v.to_datetime(&env.nls_date_format)?.unwrap())
                }
                (SqlType::Timestamp(_), v @ Value::Varchar2(_)) => {
                    Value::Timestamp(v.to_datetime(&env.nls_timestamp_format)?.unwrap())
                }
                (_, v) => v,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::like;

    #[test]
    fn like_patterns() {
        assert!(like("hello", "h%o", None).unwrap());
        assert!(like("hello", "%", None).unwrap());
        assert!(like("hello", "_ello", None).unwrap());
        assert!(!like("hello", "h_o", None).unwrap());
        assert!(like("50%", "50\\%", Some('\\')).unwrap());
        assert!(!like("500", "50\\%", Some('\\')).unwrap());
        assert!(like("abcabc", "%bc%bc", None).unwrap());
    }
}
