//! Running PL/SQL.
//!
//! The interpreter works through a [`Host`]: the session when a block runs on a
//! connection, or a read-only host when SQL calls a stored function (which, as in
//! Oracle, may not change data).

use std::collections::VecDeque;
use std::sync::Arc;

use bigdecimal::{RoundingMode, ToPrimitive};

use super::*;
use crate::ast::{BinaryOp, Expr, Statement};
use crate::catalog::{Catalog, TableColumn};
use crate::eval::{Env, Ex, Scope, VarLookup};
use crate::parser::{self, pls};
use crate::{Column, Database, OraError, Value};

/// How deep procedure and function calls may nest.
const MAX_CALL_DEPTH: u32 = 50;

/// The result of a statement run by the host.
#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub rows_affected: u64,
    /// Rows returned by a query, or the values of a DML RETURNING clause.
    pub rows: Vec<Vec<Value>>,
    /// The columns of a query.
    pub columns: Vec<Column>,
}

impl Outcome {
    fn names(&self) -> Vec<String> {
        self.columns.iter().map(|c| c.name.clone()).collect()
    }
}

/// What the interpreter needs from its surroundings.
pub(crate) trait Host {
    fn env(&self) -> &Env;
    /// Runs `f` with an evaluator over the data this host sees.
    fn with_ex<R>(
        &mut self,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
        f: impl FnOnce(&Ex) -> R,
    ) -> R;
    /// Runs a SQL statement: a query, DML, COMMIT, ROLLBACK or DDL.
    fn execute(
        &mut self,
        stmt: &Statement,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
    ) -> Result<Outcome, OraError>;
    fn routine(&self, name: &str) -> Option<Arc<Routine>>;
}

/// The host for a stored function called from SQL: it can read but not change data.
pub(crate) struct QueryHost<'a> {
    pub cat: &'a Catalog,
    pub env: &'a Env,
    pub db: &'a Database,
}

impl Host for QueryHost<'_> {
    fn env(&self) -> &Env {
        self.env
    }

    fn with_ex<R>(
        &mut self,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
        f: impl FnOnce(&Ex) -> R,
    ) -> R {
        f(&Ex {
            cat: self.cat,
            binds,
            env: self.env,
            db: self.db,
            vars,
        })
    }

    fn execute(
        &mut self,
        stmt: &Statement,
        binds: &[Value],
        vars: Option<&dyn VarLookup>,
    ) -> Result<Outcome, OraError> {
        match stmt {
            Statement::Query(q) => {
                let rel = Ex {
                    cat: self.cat,
                    binds,
                    env: self.env,
                    db: self.db,
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
            _ => Err(OraError::new(
                14551,
                "cannot perform a DML operation inside a query",
            )),
        }
    }

    fn routine(&self, name: &str) -> Option<Arc<Routine>> {
        self.db.routine(name)
    }
}

/// Runs an anonymous block. Returns the bind values after the block ran.
pub(crate) fn run_block<H: Host>(
    host: &mut H,
    block: &Block,
    binds: Vec<Value>,
) -> Result<Vec<Value>, OraError> {
    let mut it = Interp::new(host, binds);
    it.block(block).map_err(Exc::into_error)?;
    Ok(it.binds)
}

/// Calls a stored function from SQL with argument values.
pub(crate) fn call_function<H: Host>(
    host: &mut H,
    routine: &Arc<Routine>,
    args: Vec<Value>,
) -> Result<Value, OraError> {
    if !routine.is_function() {
        return Err(OraError::invalid_identifier(&format!(
            "\"{}\"",
            routine.name
        )));
    }
    if routine.params.iter().any(|p| p.mode != ParamMode::In) {
        return Err(OraError::new(
            6572,
            format!("Function {} has out arguments", routine.name),
        ));
    }
    let mut it = Interp::new(host, Vec::new());
    let args: Vec<ArgValue> = args.into_iter().map(ArgValue::In).collect();
    it.invoke(routine, args)
        .map(|(v, _)| v)
        .map_err(Exc::into_error)
}

// ---- runtime state ----

/// An exception in flight. User-defined exceptions carry their name so handlers can
/// catch them.
#[derive(Debug, Clone)]
struct Exc {
    err: OraError,
    user: Option<String>,
}

impl From<OraError> for Exc {
    fn from(err: OraError) -> Self {
        Exc { err, user: None }
    }
}

impl Exc {
    fn into_error(self) -> OraError {
        self.err
    }

    /// SQLCODE: negative Oracle error codes, +1 for user-defined exceptions and +100
    /// for NO_DATA_FOUND.
    fn sqlcode(&self) -> i64 {
        match (self.user.is_some(), self.err.code) {
            (true, 6510) => 1,
            (_, 1403) => 100,
            (_, c) => -(c as i64),
        }
    }

    fn sqlerrm(&self) -> String {
        if self.user.is_some() && self.err.code == 6510 {
            "User-Defined Exception".into()
        } else {
            self.err.to_string()
        }
    }
}

type R<T> = Result<T, Exc>;

enum Flow {
    Normal,
    Exit(Option<String>),
    Continue(Option<String>),
    Return(Option<Value>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum VarType {
    Sql(SqlType),
    Boolean,
    Integer,
    Record,
}

#[derive(Debug, Clone)]
enum Slot {
    Scalar(Value),
    /// Field names and values. Empty until the record is first filled, unless declared
    /// with a table's %ROWTYPE.
    Record(Vec<(String, Value)>),
}

#[derive(Debug, Clone)]
struct Var {
    ty: VarType,
    slot: Slot,
    constant: bool,
    not_null: bool,
}

struct OpenCursor {
    rows: VecDeque<Vec<Value>>,
    columns: Vec<String>,
    rowcount: u64,
    /// Whether the last FETCH returned a row; `None` before the first FETCH.
    found: Option<bool>,
}

struct Cursor {
    decl: CursorDecl,
    open: Option<OpenCursor>,
}

#[derive(Default)]
struct Frame {
    vars: Vec<(String, Var)>,
    /// User-defined exceptions, with the error code PRAGMA EXCEPTION_INIT gave them.
    exceptions: Vec<(String, Option<u32>)>,
    cursors: Vec<(String, Cursor)>,
    routines: Vec<Arc<Routine>>,
}

/// SQL%ROWCOUNT and friends: the last SQL statement's row count.
#[derive(Default, Clone, Copy)]
struct SqlAttrs {
    rowcount: Option<u64>,
}

/// A procedure argument after evaluation.
enum ArgValue {
    In(Value),
    /// An OUT or IN OUT argument: where the result goes, and the value passed in.
    Out(Target, Value),
}

/// The variables a SQL expression sees, borrowed from the interpreter.
struct Vars<'a> {
    frames: &'a [Frame],
    sql: SqlAttrs,
    handling: Option<&'a Exc>,
}

impl VarLookup for Vars<'_> {
    fn lookup(&self, qualifier: Option<&str>, name: &str) -> Option<Value> {
        let boolean = |b: Option<bool>| b.map_or(Value::Null, Value::Boolean);
        match qualifier {
            None => {
                for f in self.frames.iter().rev() {
                    if let Some((_, v)) = f.vars.iter().rev().find(|(n, _)| n == name) {
                        return match &v.slot {
                            Slot::Scalar(v) => Some(v.clone()),
                            Slot::Record(_) => None,
                        };
                    }
                }
                match name {
                    "SQLCODE" => Some(Value::number(self.handling.map_or(0, Exc::sqlcode))),
                    "SQLERRM" => Some(Value::varchar(self.handling.map_or_else(
                        || "ORA-0000: normal, successful completion".to_string(),
                        Exc::sqlerrm,
                    ))),
                    _ => None,
                }
            }
            Some("SQL%") => Some(match name {
                "ROWCOUNT" => self.sql.rowcount.map_or(Value::Null, Value::number),
                "FOUND" => boolean(self.sql.rowcount.map(|n| n > 0)),
                "NOTFOUND" => boolean(self.sql.rowcount.map(|n| n == 0)),
                "ISOPEN" => Value::Boolean(false),
                _ => return None,
            }),
            Some(q) if q.ends_with('%') => {
                let cursor = &q[..q.len() - 1];
                let c = self
                    .frames
                    .iter()
                    .rev()
                    .find_map(|f| f.cursors.iter().rev().find(|(n, _)| n == cursor))?;
                let open = c.1.open.as_ref();
                Some(match name {
                    "ISOPEN" => Value::Boolean(open.is_some()),
                    // Other attributes of a closed cursor raise INVALID_CURSOR; NULL is close enough.
                    "FOUND" => boolean(open.and_then(|o| o.found)),
                    "NOTFOUND" => boolean(open.and_then(|o| o.found.map(|f| !f))),
                    "ROWCOUNT" => open.map_or(Value::Null, |o| Value::number(o.rowcount)),
                    _ => return None,
                })
            }
            Some(record) => {
                for f in self.frames.iter().rev() {
                    if let Some((_, v)) = f.vars.iter().rev().find(|(n, _)| n == record) {
                        return match &v.slot {
                            Slot::Record(fields) => fields
                                .iter()
                                .find(|(n, _)| n == name)
                                .map(|(_, v)| v.clone()),
                            Slot::Scalar(_) => None,
                        };
                    }
                }
                None
            }
        }
    }
}

// ---- errors ----

fn not_declared(name: &str) -> OraError {
    pls(201, &format!("identifier '{name}' must be declared"))
}

/// In PL/SQL an unknown name is a compile error rather than ORA-00904.
fn undeclared(e: OraError) -> OraError {
    if e.code != 904 {
        return e;
    }
    let name = e.message.split(':').next().unwrap_or("").trim_matches('"');
    not_declared(name)
}

fn value_error(detail: &str) -> OraError {
    OraError::new(6502, format!("PL/SQL: numeric or value error{detail}"))
}

fn wrong_type() -> OraError {
    pls(382, "expression is of wrong type")
}

fn no_data_found() -> OraError {
    OraError::new(1403, "no data found")
}

fn too_many_rows() -> OraError {
    OraError::new(
        1422,
        "exact fetch returns more than requested number of rows",
    )
}

fn invalid_cursor() -> OraError {
    OraError::new(1001, "invalid cursor")
}

/// The error code of a predefined exception name.
fn predefined(name: &str) -> Option<(u32, &'static str)> {
    Some(match name {
        "NO_DATA_FOUND" => (1403, "no data found"),
        "TOO_MANY_ROWS" => (
            1422,
            "exact fetch returns more than requested number of rows",
        ),
        "DUP_VAL_ON_INDEX" => (1, "unique constraint violated"),
        "ZERO_DIVIDE" => (1476, "divisor is equal to zero"),
        "VALUE_ERROR" => (6502, "PL/SQL: numeric or value error"),
        "INVALID_NUMBER" => (1722, "invalid number"),
        "INVALID_CURSOR" => (1001, "invalid cursor"),
        "CURSOR_ALREADY_OPEN" => (6511, "PL/SQL: cursor already open"),
        "CASE_NOT_FOUND" => (6592, "CASE not found while executing CASE statement"),
        "PROGRAM_ERROR" => (6501, "PL/SQL: program error"),
        "STORAGE_ERROR" => (6500, "PL/SQL: storage error"),
        "LOGIN_DENIED" => (1017, "invalid username/password; logon denied"),
        "NOT_LOGGED_ON" => (1012, "not logged on"),
        "TIMEOUT_ON_RESOURCE" => (51, "timeout occurred while waiting for a resource"),
        "ROWTYPE_MISMATCH" => (
            6504,
            "PL/SQL: Return types of Result Set variables or query do not match",
        ),
        _ => return None,
    })
}

/// Converts errors from storing into a column into PL/SQL's VALUE_ERROR.
fn as_value_error(e: OraError) -> OraError {
    match e.code {
        12899 => value_error(": character string buffer too small"),
        1438 => value_error(": number precision too large"),
        1722 => value_error(": character to number conversion error"),
        _ => e,
    }
}

// ---- the interpreter ----

struct Interp<'h, H: Host> {
    host: &'h mut H,
    binds: Vec<Value>,
    frames: Vec<Frame>,
    sql: SqlAttrs,
    /// Exceptions being handled, innermost last, for SQLCODE, SQLERRM and RAISE.
    handling: Vec<Exc>,
}

impl<'h, H: Host> Interp<'h, H> {
    fn new(host: &'h mut H, binds: Vec<Value>) -> Self {
        Interp {
            host,
            binds,
            frames: Vec::new(),
            sql: SqlAttrs::default(),
            handling: Vec::new(),
        }
    }

    // ---- expressions ----

    fn eval(&mut self, e: &Expr) -> R<Value> {
        let e = self.resolve_calls(e)?;
        let vars = Vars {
            frames: &self.frames,
            sql: self.sql,
            handling: self.handling.last(),
        };
        Ok(self
            .host
            .with_ex(&self.binds, Some(&vars), |ex| {
                ex.eval(&e, &Scope::empty(None))
            })
            .map_err(undeclared)?)
    }

    fn eval_bool(&mut self, e: &Expr) -> R<Option<bool>> {
        // Short-circuit so calls in operands that do not decide the result never run.
        if self.has_call(e) {
            match e {
                Expr::Binary(BinaryOp::And, a, b) => {
                    let l = self.eval_bool(a)?;
                    if l == Some(false) {
                        return Ok(Some(false));
                    }
                    return Ok(match (l, self.eval_bool(b)?) {
                        (_, Some(false)) => Some(false),
                        (Some(true), Some(true)) => Some(true),
                        _ => None,
                    });
                }
                Expr::Binary(BinaryOp::Or, a, b) => {
                    let l = self.eval_bool(a)?;
                    if l == Some(true) {
                        return Ok(Some(true));
                    }
                    return Ok(match (l, self.eval_bool(b)?) {
                        (_, Some(true)) => Some(true),
                        (Some(false), Some(false)) => Some(false),
                        _ => None,
                    });
                }
                Expr::Not(a) => return Ok(self.eval_bool(a)?.map(|b| !b)),
                _ => {}
            }
        }
        let e = self.resolve_calls(e)?;
        let vars = Vars {
            frames: &self.frames,
            sql: self.sql,
            handling: self.handling.last(),
        };
        Ok(self
            .host
            .with_ex(&self.binds, Some(&vars), |ex| {
                ex.eval_bool(&e, &Scope::empty(None))
            })
            .map_err(undeclared)?)
    }

    fn eval_int(&mut self, e: &Expr) -> R<i64> {
        let v = self.eval(e)?;
        let n = v
            .to_number()
            .map_err(as_value_error)?
            .ok_or_else(|| value_error(""))?;
        n.with_scale_round(0, RoundingMode::HalfUp)
            .to_i64()
            .ok_or_else(|| Exc::from(value_error("")))
    }

    /// Whether two values are equal, as CASE and DECODE compare them.
    fn equal(&self, a: &Value, b: &Value) -> R<bool> {
        let fmt = &self.host.env().nls_date_format;
        Ok(crate::value::compare(a, b, fmt)? == Some(std::cmp::Ordering::Equal))
    }

    /// Evaluates an expression whose operands are evaluated only when needed: CASE,
    /// AND, OR, NVL, NVL2, COALESCE and DECODE. Returns `None` for other expressions.
    fn eval_lazy(&mut self, e: &Expr) -> R<Option<Value>> {
        Ok(Some(match e {
            Expr::Binary(BinaryOp::And | BinaryOp::Or, ..) => {
                self.eval_bool(e)?.map_or(Value::Null, Value::Boolean)
            }
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o)?),
                    None => None,
                };
                for (when, then) in whens {
                    let hit = match &subject {
                        Some(s) => {
                            let w = self.eval(when)?;
                            self.equal(s, &w)?
                        }
                        None => self.eval_bool(when)? == Some(true),
                    };
                    if hit {
                        return Ok(Some(self.eval(then)?));
                    }
                }
                match else_ {
                    Some(x) => self.eval(x)?,
                    None => Value::Null,
                }
            }
            Expr::Function { name, args, .. } if self.find_routine(name).is_none() => {
                match (name.as_str(), args.len()) {
                    ("NVL", 2) | ("COALESCE", 1..) => {
                        for a in args {
                            let v = self.eval(a)?;
                            if !v.is_null() {
                                return Ok(Some(v));
                            }
                        }
                        Value::Null
                    }
                    ("NVL2", 3) => {
                        if self.eval(&args[0])?.is_null() {
                            self.eval(&args[2])?
                        } else {
                            self.eval(&args[1])?
                        }
                    }
                    ("DECODE", 3..) => {
                        let v = self.eval(&args[0])?;
                        let mut rest = args[1..].chunks(2);
                        for pair in rest.by_ref() {
                            let [search, result] = pair else {
                                // The odd one out is the default.
                                return Ok(Some(self.eval(&pair[0])?));
                            };
                            let s = self.eval(search)?;
                            if (v.is_null() && s.is_null()) || self.equal(&v, &s)? {
                                return Ok(Some(self.eval(result)?));
                            }
                        }
                        Value::Null
                    }
                    _ => return Ok(None),
                }
            }
            _ => return Ok(None),
        }))
    }

    /// Calls to procedures and functions in a PL/SQL expression run here, with full
    /// access to the session, and are replaced by their results. Calls inside
    /// subqueries are left to SQL, where functions may only read. Calls in operands
    /// that CASE, AND, OR and the NULL functions skip do not run.
    fn resolve_calls(&mut self, e: &Expr) -> R<Expr> {
        if !self.has_call(e) {
            return Ok(e.clone());
        }
        if let Some(v) = self.eval_lazy(e)? {
            return Ok(Expr::Literal(v));
        }
        Ok(match e {
            Expr::Column { table: None, name } => match self.find_routine(name) {
                Some(r) => Expr::Literal(self.call(&r, &[])?.unwrap_or(Value::Null)),
                None => e.clone(),
            },
            Expr::Function {
                name,
                args,
                distinct,
            } => {
                if let Some(r) = self.find_routine(name) {
                    let mut values = Vec::with_capacity(args.len());
                    for a in args {
                        let a = self.resolve_calls(a)?;
                        values.push(Arg {
                            name: None,
                            value: a,
                        });
                    }
                    let v = self.call(&r, &values)?;
                    return Ok(Expr::Literal(v.unwrap_or(Value::Null)));
                }
                Expr::Function {
                    name: name.clone(),
                    args: args
                        .iter()
                        .map(|x| self.resolve_calls(x))
                        .collect::<R<_>>()?,
                    distinct: *distinct,
                }
            }
            Expr::Neg(a) => Expr::Neg(Box::new(self.resolve_calls(a)?)),
            Expr::Not(a) => Expr::Not(Box::new(self.resolve_calls(a)?)),
            Expr::Binary(op, a, b) => Expr::Binary(
                *op,
                Box::new(self.resolve_calls(a)?),
                Box::new(self.resolve_calls(b)?),
            ),
            Expr::IsNull { expr, negated } => Expr::IsNull {
                expr: Box::new(self.resolve_calls(expr)?),
                negated: *negated,
            },
            Expr::Between {
                expr,
                low,
                high,
                negated,
            } => Expr::Between {
                expr: Box::new(self.resolve_calls(expr)?),
                low: Box::new(self.resolve_calls(low)?),
                high: Box::new(self.resolve_calls(high)?),
                negated: *negated,
            },
            Expr::InList {
                expr,
                list,
                negated,
            } => Expr::InList {
                expr: Box::new(self.resolve_calls(expr)?),
                list: list
                    .iter()
                    .map(|x| self.resolve_calls(x))
                    .collect::<R<_>>()?,
                negated: *negated,
            },
            Expr::Like {
                expr,
                pattern,
                escape,
                negated,
            } => Expr::Like {
                expr: Box::new(self.resolve_calls(expr)?),
                pattern: Box::new(self.resolve_calls(pattern)?),
                escape: match escape {
                    Some(x) => Some(Box::new(self.resolve_calls(x)?)),
                    None => None,
                },
                negated: *negated,
            },
            Expr::Case {
                operand,
                whens,
                else_,
            } => Expr::Case {
                operand: match operand {
                    Some(x) => Some(Box::new(self.resolve_calls(x)?)),
                    None => None,
                },
                whens: whens
                    .iter()
                    .map(|(w, t)| Ok((self.resolve_calls(w)?, self.resolve_calls(t)?)))
                    .collect::<R<_>>()?,
                else_: match else_ {
                    Some(x) => Some(Box::new(self.resolve_calls(x)?)),
                    None => None,
                },
            },
            other => other.clone(),
        })
    }

    /// Whether an expression calls a procedure or function outside any subquery.
    fn has_call(&self, e: &Expr) -> bool {
        let rec = |x: &Expr| self.has_call(x);
        match e {
            Expr::Column { table: None, name } => {
                !self.is_var(name) && self.find_routine(name).is_some()
            }
            Expr::Function { name, args, .. } => {
                self.find_routine(name).is_some() || args.iter().any(rec)
            }
            Expr::Neg(a) | Expr::Not(a) => rec(a),
            Expr::Binary(_, a, b) => rec(a) || rec(b),
            Expr::IsNull { expr, .. } => rec(expr),
            Expr::Between {
                expr, low, high, ..
            } => rec(expr) || rec(low) || rec(high),
            Expr::InList { expr, list, .. } => rec(expr) || list.iter().any(rec),
            Expr::Like {
                expr,
                pattern,
                escape,
                ..
            } => rec(expr) || rec(pattern) || escape.as_deref().is_some_and(rec),
            Expr::Case {
                operand,
                whens,
                else_,
            } => {
                operand.as_deref().is_some_and(rec)
                    || whens.iter().any(|(w, t)| rec(w) || rec(t))
                    || else_.as_deref().is_some_and(rec)
            }
            _ => false,
        }
    }

    fn is_var(&self, name: &str) -> bool {
        self.frames
            .iter()
            .any(|f| f.vars.iter().any(|(n, _)| n == name))
    }

    /// A local or stored procedure or function. A schema prefix is ignored.
    fn find_routine(&self, name: &str) -> Option<Arc<Routine>> {
        for f in self.frames.iter().rev() {
            if let Some(r) = f.routines.iter().rev().find(|r| r.name == name) {
                return Some(r.clone());
            }
        }
        let short = name.rsplit('.').next().unwrap_or(name);
        if short != name && name.split('.').count() > 2 {
            return None;
        }
        self.host.routine(short)
    }

    // ---- variables ----

    fn declare(&mut self, name: &str, var: Var) {
        self.frames
            .last_mut()
            .expect("a frame")
            .vars
            .push((name.to_string(), var));
    }

    fn find_var(&mut self, name: &str) -> Option<&mut Var> {
        self.frames
            .iter_mut()
            .rev()
            .find_map(|f| f.vars.iter_mut().rev().find(|(n, _)| n == name))
            .map(|(_, v)| v)
    }

    /// Resolves a declared type.
    fn var_type(&mut self, ty: &TypeRef) -> R<(VarType, Slot)> {
        Ok(match ty {
            TypeRef::Sql(t) => (VarType::Sql(*t), Slot::Scalar(Value::Null)),
            TypeRef::Boolean => (VarType::Boolean, Slot::Scalar(Value::Null)),
            TypeRef::Integer => (VarType::Integer, Slot::Scalar(Value::Null)),
            TypeRef::TypeOf {
                table: Some(table),
                name,
            } => {
                let t = self.host.with_ex(&[], None, |ex| {
                    ex.cat
                        .table(table)
                        .and_then(|t| t.column_index(name).map(|i| t.columns[i].sql_type))
                });
                match t {
                    Some(t) => (VarType::Sql(t), Slot::Scalar(Value::Null)),
                    None => return Err(not_declared(&format!("{table}.{name}")).into()),
                }
            }
            TypeRef::TypeOf { table: None, name } => match self.find_var(name) {
                Some(v) => match &v.slot {
                    Slot::Scalar(_) => (v.ty, Slot::Scalar(Value::Null)),
                    Slot::Record(fields) => (
                        VarType::Record,
                        Slot::Record(
                            fields
                                .iter()
                                .map(|(n, _)| (n.clone(), Value::Null))
                                .collect(),
                        ),
                    ),
                },
                None => return Err(not_declared(name).into()),
            },
            TypeRef::RowType(name) => {
                let cursor = self
                    .frames
                    .iter()
                    .any(|f| f.cursors.iter().any(|(n, _)| n == name));
                if cursor {
                    // Fields come from the cursor's query when the record is first filled.
                    (VarType::Record, Slot::Record(Vec::new()))
                } else {
                    let cols = self.host.with_ex(&[], None, |ex| {
                        ex.cat.table(name).map(|t| {
                            t.columns
                                .iter()
                                .map(|c| (c.name.clone(), Value::Null))
                                .collect::<Vec<_>>()
                        })
                    });
                    match cols {
                        Some(cols) => (VarType::Record, Slot::Record(cols)),
                        None => return Err(not_declared(name).into()),
                    }
                }
            }
        })
    }

    /// Converts a value for a variable of type `ty`.
    fn convert(&self, ty: VarType, v: Value) -> R<Value> {
        if v.is_null() {
            return Ok(Value::Null);
        }
        Ok(match ty {
            VarType::Record => return Err(wrong_type().into()),
            VarType::Boolean => match v {
                Value::Boolean(_) => v,
                _ => return Err(wrong_type().into()),
            },
            VarType::Integer => {
                if let Value::Boolean(_) = v {
                    return Err(wrong_type().into());
                }
                let n = v.to_number().map_err(as_value_error)?.unwrap();
                Value::Number(n.with_scale_round(0, RoundingMode::HalfUp))
            }
            VarType::Sql(t) => {
                if let Value::Boolean(_) = v {
                    return Err(wrong_type().into());
                }
                let env = self.host.env();
                let col = TableColumn {
                    name: String::new(),
                    sql_type: t,
                    default: None,
                    not_null: false,
                    identity: false,
                };
                crate::session::store_value(env, &env.user, "", &col, v).map_err(as_value_error)?
            }
        })
    }

    fn assign(&mut self, target: &Target, v: Value) -> R<()> {
        match target {
            Target::Bind(i) => {
                if *i >= self.binds.len() {
                    self.binds.resize(*i + 1, Value::Null);
                }
                self.binds[*i] = v;
                Ok(())
            }
            Target::Var(name) => {
                let Some(var) = self.find_var(name) else {
                    return Err(not_declared(name).into());
                };
                if var.constant {
                    return Err(pls(
                        363,
                        &format!("expression '{name}' cannot be used as an assignment target"),
                    )
                    .into());
                }
                let ty = var.ty;
                let not_null = var.not_null;
                let v = self.convert(ty, v)?;
                if not_null && v.is_null() {
                    return Err(value_error("").into());
                }
                let var = self.find_var(name).unwrap();
                var.slot = Slot::Scalar(v);
                Ok(())
            }
            Target::Field(record, field) => {
                let Some(var) = self.find_var(record) else {
                    return Err(not_declared(record).into());
                };
                match &mut var.slot {
                    Slot::Record(fields) => {
                        let empty = fields.is_empty();
                        match fields.iter_mut().find(|(n, _)| n == field) {
                            Some((_, slot)) => *slot = v,
                            None if empty => fields.push((field.clone(), v)),
                            None => {
                                return Err(pls(
                                    302,
                                    &format!("component '{field}' must be declared"),
                                )
                                .into())
                            }
                        }
                        Ok(())
                    }
                    Slot::Scalar(_) => {
                        Err(pls(302, &format!("component '{field}' must be declared")).into())
                    }
                }
            }
        }
    }

    /// Assigns one row to INTO targets: a single record, or one target per column.
    fn assign_row(&mut self, targets: &[Target], columns: &[String], row: Vec<Value>) -> R<()> {
        if let [Target::Var(name)] = targets {
            if let Some(var) = self.find_var(name) {
                if let Slot::Record(fields) = &mut var.slot {
                    if fields.len() == row.len() {
                        for ((_, slot), v) in fields.iter_mut().zip(row) {
                            *slot = v;
                        }
                    } else {
                        *fields = columns.iter().cloned().zip(row).collect();
                    }
                    return Ok(());
                }
            }
        }
        match targets.len().cmp(&row.len()) {
            std::cmp::Ordering::Less => return Err(OraError::new(913, "too many values").into()),
            std::cmp::Ordering::Greater => {
                return Err(OraError::new(947, "not enough values").into())
            }
            _ => {}
        }
        for (t, v) in targets.iter().zip(row) {
            self.assign(t, v)?;
        }
        Ok(())
    }

    fn declare_all(&mut self, decls: &[Decl]) -> R<()> {
        for d in decls {
            match d {
                Decl::Var(v) => {
                    let (ty, mut slot) = self.var_type(&v.ty)?;
                    if let Some(e) = &v.default {
                        let value = self.eval(e)?;
                        slot = Slot::Scalar(self.convert(ty, value)?);
                    }
                    if v.not_null && matches!(slot, Slot::Scalar(Value::Null)) {
                        return Err(value_error("").into());
                    }
                    self.declare(
                        &v.name,
                        Var {
                            ty,
                            slot,
                            constant: v.constant,
                            not_null: v.not_null,
                        },
                    );
                }
                Decl::Exception(name) => {
                    let f = self.frames.last_mut().unwrap();
                    f.exceptions.push((name.clone(), None));
                }
                Decl::ExceptionInit { name, code } => {
                    let found = self
                        .frames
                        .iter_mut()
                        .rev()
                        .find_map(|f| f.exceptions.iter_mut().rev().find(|(n, _)| n == name));
                    match found {
                        Some((_, c)) => *c = Some(*code),
                        None => return Err(not_declared(name).into()),
                    }
                }
                Decl::Cursor(c) => {
                    let f = self.frames.last_mut().unwrap();
                    f.cursors.push((
                        c.name.clone(),
                        Cursor {
                            decl: c.clone(),
                            open: None,
                        },
                    ));
                }
                Decl::Routine(r) => self.frames.last_mut().unwrap().routines.push(r.clone()),
            }
        }
        Ok(())
    }

    // ---- blocks and statements ----

    fn block(&mut self, b: &Block) -> R<Flow> {
        self.frames.push(Frame::default());
        let result = self.block_in_frame(b);
        self.frames.pop();
        result
    }

    fn block_in_frame(&mut self, b: &Block) -> R<Flow> {
        // Errors while declaring are not caught by this block's handlers.
        self.declare_all(&b.decls)?;
        match self.statements(&b.body) {
            Err(exc) if !b.handlers.is_empty() => self.handle(b, exc),
            other => other,
        }
    }

    fn handle(&mut self, b: &Block, exc: Exc) -> R<Flow> {
        for h in &b.handlers {
            if h.exceptions.iter().any(|name| self.matches(name, &exc)) {
                self.handling.push(exc);
                let result = self.statements(&h.body);
                self.handling.pop();
                return result;
            }
        }
        Err(exc)
    }

    fn matches(&self, name: &str, exc: &Exc) -> bool {
        if name == "OTHERS" {
            return true;
        }
        let user = self
            .frames
            .iter()
            .rev()
            .find_map(|f| f.exceptions.iter().rev().find(|(n, _)| n == name));
        match user {
            Some((_, Some(code))) => exc.err.code == *code,
            Some((_, None)) => exc.user.as_deref() == Some(name),
            None => {
                let short = name.rsplit('.').next().unwrap_or(name);
                predefined(short).is_some_and(|(code, _)| exc.err.code == code)
            }
        }
    }

    fn statements(&mut self, stmts: &[Stmt]) -> R<Flow> {
        for s in stmts {
            match self.statement(s)? {
                Flow::Normal => {}
                flow => return Ok(flow),
            }
        }
        Ok(Flow::Normal)
    }

    /// Runs a loop body once. Returns `Some(flow)` when the loop should stop.
    fn loop_iteration(&mut self, label: &Option<String>, body: &[Stmt]) -> R<Option<Flow>> {
        let mine = |l: &Option<String>| l.is_none() || l == label;
        Ok(match self.statements(body)? {
            Flow::Normal => None,
            Flow::Continue(l) if mine(&l) => None,
            Flow::Exit(l) if mine(&l) => Some(Flow::Normal),
            flow => Some(flow),
        })
    }

    fn statement(&mut self, s: &Stmt) -> R<Flow> {
        match s {
            Stmt::Null => {}
            Stmt::Assign { target, value } => {
                let v = self.eval(value)?;
                self.assign(target, v)?;
            }
            Stmt::If { branches, else_ } => {
                for (cond, body) in branches {
                    if self.eval_bool(cond)? == Some(true) {
                        return self.statements(body);
                    }
                }
                if let Some(body) = else_ {
                    return self.statements(body);
                }
            }
            Stmt::Case {
                operand,
                whens,
                else_,
            } => {
                let subject = match operand {
                    Some(o) => Some(self.eval(o)?),
                    None => None,
                };
                for (when, body) in whens {
                    let hit = match &subject {
                        Some(s) => {
                            let w = self.eval(when)?;
                            let fmt = self.host.env().nls_date_format.clone();
                            crate::value::compare(s, &w, &fmt)? == Some(std::cmp::Ordering::Equal)
                        }
                        None => self.eval_bool(when)? == Some(true),
                    };
                    if hit {
                        return self.statements(body);
                    }
                }
                match else_ {
                    Some(body) => return self.statements(body),
                    None => {
                        return Err(OraError::new(
                            6592,
                            "CASE not found while executing CASE statement",
                        )
                        .into())
                    }
                }
            }
            Stmt::Loop { label, body } => loop {
                if let Some(flow) = self.loop_iteration(label, body)? {
                    return Ok(flow);
                }
            },
            Stmt::While { label, cond, body } => {
                while self.eval_bool(cond)? == Some(true) {
                    if let Some(flow) = self.loop_iteration(label, body)? {
                        return Ok(flow);
                    }
                }
            }
            Stmt::ForRange {
                label,
                var,
                reverse,
                low,
                high,
                body,
            } => {
                let (lo, hi) = (self.eval_int(low)?, self.eval_int(high)?);
                self.frames.push(Frame::default());
                let result = (|| {
                    let mut i = if *reverse { hi } else { lo };
                    while lo <= i && i <= hi {
                        self.frames.last_mut().unwrap().vars = vec![(
                            var.clone(),
                            Var {
                                ty: VarType::Integer,
                                slot: Slot::Scalar(Value::number(i)),
                                constant: true,
                                not_null: true,
                            },
                        )];
                        if let Some(flow) = self.loop_iteration(label, body)? {
                            return Ok(flow);
                        }
                        i += if *reverse { -1 } else { 1 };
                    }
                    Ok(Flow::Normal)
                })();
                self.frames.pop();
                return result;
            }
            Stmt::ForCursor {
                label,
                var,
                source,
                body,
            } => {
                let (columns, rows) = match source {
                    CursorSource::Query(q) => {
                        let o = self.run_sql(&Statement::Query(q.clone()))?;
                        (o.names(), o.rows)
                    }
                    CursorSource::Named { name, args } => {
                        let o = self.open_query(name, args)?;
                        (o.columns, o.rows.into())
                    }
                };
                self.frames.push(Frame::default());
                let result = (|| {
                    for row in rows {
                        self.frames.last_mut().unwrap().vars = vec![(
                            var.clone(),
                            Var {
                                ty: VarType::Record,
                                slot: Slot::Record(columns.iter().cloned().zip(row).collect()),
                                constant: false,
                                not_null: false,
                            },
                        )];
                        if let Some(flow) = self.loop_iteration(label, body)? {
                            return Ok(flow);
                        }
                    }
                    Ok(Flow::Normal)
                })();
                self.frames.pop();
                return result;
            }
            Stmt::Exit { label, when } | Stmt::Continue { label, when } => {
                let go = match when {
                    Some(c) => self.eval_bool(c)? == Some(true),
                    None => true,
                };
                if go {
                    return Ok(if matches!(s, Stmt::Exit { .. }) {
                        Flow::Exit(label.clone())
                    } else {
                        Flow::Continue(label.clone())
                    });
                }
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => Some(self.eval(e)?),
                    None => None,
                };
                return Ok(Flow::Return(v));
            }
            Stmt::Raise(None) => return Err(match self.handling.last() {
                Some(exc) => exc.clone(),
                None => pls(
                    367,
                    "a RAISE statement with no exception name must be inside an exception handler",
                )
                .into(),
            }),
            Stmt::Raise(Some(name)) => return Err(self.raise(name)),
            Stmt::Sql { stmt, into } => {
                let o = self.run_sql(stmt)?;
                match stmt {
                    Statement::Query(_) => {
                        let names = o.names();
                        let mut rows = o.rows.into_iter();
                        let Some(row) = rows.next() else {
                            self.sql.rowcount = Some(0);
                            return Err(no_data_found().into());
                        };
                        if rows.next().is_some() {
                            self.sql.rowcount = Some(2);
                            return Err(too_many_rows().into());
                        }
                        self.sql.rowcount = Some(1);
                        self.assign_row(into, &names, row)?;
                    }
                    Statement::Insert(crate::ast::Insert { returning, .. })
                    | Statement::Update(crate::ast::Update { returning, .. })
                    | Statement::Delete(crate::ast::Delete { returning, .. }) => {
                        self.sql.rowcount = Some(o.rows_affected);
                        if let Some(r) = returning {
                            self.assign_returning(&r.into, o.rows)?;
                        }
                    }
                    _ => {}
                }
            }
            Stmt::Call { name, args } => {
                self.call_statement(name, args)?;
            }
            Stmt::ExecuteImmediate { sql, into, using } => {
                self.execute_immediate(sql, into, using)?;
            }
            Stmt::Open { cursor, args } => {
                if self.cursor_mut(cursor)?.open.is_some() {
                    return Err(OraError::new(6511, "PL/SQL: cursor already open").into());
                }
                let open = self.open_query(cursor, args)?;
                self.cursor_mut(cursor)?.open = Some(open);
            }
            Stmt::Fetch { cursor, into } => {
                let c = self.cursor_mut(cursor)?;
                let Some(open) = c.open.as_mut() else {
                    return Err(invalid_cursor().into());
                };
                let row = open.rows.pop_front();
                open.found = Some(row.is_some());
                if row.is_some() {
                    open.rowcount += 1;
                }
                let columns = open.columns.clone();
                if let Some(row) = row {
                    self.assign_row(into, &columns, row)?;
                }
            }
            Stmt::Close { cursor } => {
                let c = self.cursor_mut(cursor)?;
                if c.open.take().is_none() {
                    return Err(invalid_cursor().into());
                }
            }
            Stmt::Block(b) => return self.block(b),
        }
        Ok(Flow::Normal)
    }

    fn raise(&self, name: &str) -> Exc {
        let user = self
            .frames
            .iter()
            .rev()
            .find_map(|f| f.exceptions.iter().rev().find(|(n, _)| n == name));
        match user {
            Some((_, Some(code))) => Exc {
                err: OraError::new(*code, ""),
                user: Some(name.to_string()),
            },
            Some((_, None)) => Exc {
                err: OraError::new(6510, "PL/SQL: unhandled user-defined exception"),
                user: Some(name.to_string()),
            },
            None => {
                let short = name.rsplit('.').next().unwrap_or(name);
                match predefined(short) {
                    Some((code, message)) => OraError::new(code, message).into(),
                    None => not_declared(name).into(),
                }
            }
        }
    }

    /// Runs a SQL statement with the block's variables and binds visible.
    fn run_sql(&mut self, stmt: &Statement) -> R<Outcome> {
        let vars = Vars {
            frames: &self.frames,
            sql: self.sql,
            handling: self.handling.last(),
        };
        Ok(self.host.execute(stmt, &self.binds, Some(&vars))?)
    }

    /// Stores DML RETURNING values in their targets: one row at most.
    fn assign_returning(&mut self, into: &[Expr], rows: Vec<Vec<Value>>) -> R<()> {
        if rows.len() > 1 {
            return Err(too_many_rows().into());
        }
        let row = rows
            .into_iter()
            .next()
            .unwrap_or_else(|| vec![Value::Null; into.len()]);
        for (e, v) in into.iter().zip(row) {
            let t = match e {
                Expr::Bind(i) => Target::Bind(*i),
                Expr::Column { table: None, name } => Target::Var(name.clone()),
                Expr::Column {
                    table: Some(r),
                    name,
                } => Target::Field(r.clone(), name.clone()),
                _ => return Err(wrong_type().into()),
            };
            self.assign(&t, v)?;
        }
        Ok(())
    }

    fn cursor_mut(&mut self, name: &str) -> R<&mut Cursor> {
        self.frames
            .iter_mut()
            .rev()
            .find_map(|f| f.cursors.iter_mut().rev().find(|(n, _)| n == name))
            .map(|(_, c)| c)
            .ok_or_else(|| not_declared(name).into())
    }

    /// Runs a declared cursor's query with its parameters bound.
    fn open_query(&mut self, name: &str, args: &[Expr]) -> R<OpenCursor> {
        let decl = self.cursor_mut(name)?.decl.clone();
        if args.len() > decl.params.len() {
            return Err(pls(
                306,
                &format!("wrong number or types of arguments in call to '{name}'"),
            )
            .into());
        }
        let mut params = Vec::new();
        for (i, p) in decl.params.iter().enumerate() {
            let v = match (args.get(i), &p.default) {
                (Some(a), _) => self.eval(a)?,
                (None, Some(d)) => self.eval(d)?,
                (None, None) => {
                    return Err(pls(
                        306,
                        &format!("wrong number or types of arguments in call to '{name}'"),
                    )
                    .into())
                }
            };
            let (ty, _) = self.var_type(&p.ty)?;
            let v = self.convert(ty, v)?;
            params.push((
                p.name.clone(),
                Var {
                    ty,
                    slot: Slot::Scalar(v),
                    constant: true,
                    not_null: false,
                },
            ));
        }
        self.frames.push(Frame {
            vars: params,
            ..Frame::default()
        });
        let o = self.run_sql(&Statement::Query(decl.query.clone()));
        self.frames.pop();
        let o = o?;
        Ok(OpenCursor {
            columns: o.names(),
            rows: o.rows.into(),
            rowcount: 0,
            found: None,
        })
    }

    fn execute_immediate(&mut self, sql: &Expr, into: &[Target], using: &[Expr]) -> R<()> {
        let text = match self.eval(sql)? {
            Value::Null => return Err(OraError::new(900, "invalid SQL statement").into()),
            v => v.to_string(),
        };
        let mut binds = Vec::with_capacity(using.len());
        for e in using {
            binds.push(self.eval(e)?);
        }
        let parsed = parser::parse(text.trim())?;
        if binds.len() < parsed.binds {
            return Err(OraError::new(1008, "not all variables bound").into());
        }
        match &parsed.stmt {
            Statement::Block(b) => {
                // Dynamic PL/SQL runs on its own, seeing only its binds.
                let mut inner = Interp::new(&mut *self.host, binds);
                inner.block(b)?;
            }
            stmt => {
                // Dynamic SQL does not see the block's variables.
                let o = self.host.execute(stmt, &binds, None)?;
                match stmt {
                    Statement::Query(_) if !into.is_empty() => {
                        let names = o.names();
                        let mut rows = o.rows.into_iter();
                        let row = rows.next().ok_or_else(no_data_found)?;
                        if rows.next().is_some() {
                            return Err(too_many_rows().into());
                        }
                        self.sql.rowcount = Some(1);
                        self.assign_row(into, &names, row)?;
                    }
                    Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => {
                        self.sql.rowcount = Some(o.rows_affected);
                    }
                    _ => self.sql.rowcount = Some(0),
                }
            }
        }
        Ok(())
    }

    // ---- calls ----

    fn call_statement(&mut self, name: &str, args: &[Arg]) -> R<()> {
        if self.builtin(name, args)? {
            return Ok(());
        }
        let Some(r) = self.find_routine(name) else {
            return Err(not_declared(name).into());
        };
        if r.is_function() {
            return Err(pls(
                221,
                &format!("'{}' is not a procedure or is undefined", r.name),
            )
            .into());
        }
        self.call(&r, args)?;
        Ok(())
    }

    /// Calls a procedure or function with argument expressions, then stores OUT
    /// arguments. Returns a function's result.
    fn call(&mut self, r: &Arc<Routine>, args: &[Arg]) -> R<Option<Value>> {
        let wrong_args = || -> Exc {
            pls(
                306,
                &format!("wrong number or types of arguments in call to '{}'", r.name),
            )
            .into()
        };
        // Match arguments to parameters: positional first, then named.
        let mut slots: Vec<Option<&Expr>> = vec![None; r.params.len()];
        let mut named = false;
        for (i, a) in args.iter().enumerate() {
            match &a.name {
                None if named => return Err(wrong_args()),
                None => *slots.get_mut(i).ok_or_else(wrong_args)? = Some(&a.value),
                Some(n) => {
                    named = true;
                    let p = r
                        .params
                        .iter()
                        .position(|p| &p.name == n)
                        .ok_or_else(wrong_args)?;
                    if slots[p].is_some() {
                        return Err(wrong_args());
                    }
                    slots[p] = Some(&a.value);
                }
            }
        }
        let mut values = Vec::with_capacity(r.params.len());
        for (p, arg) in r.params.iter().zip(slots) {
            values.push(match (p.mode, arg) {
                (ParamMode::In, Some(e)) => ArgValue::In(self.eval(e)?),
                (ParamMode::In, None) => match &p.default {
                    Some(d) => ArgValue::In(self.eval(d)?),
                    None => return Err(wrong_args()),
                },
                (mode, Some(e)) => {
                    let t = match e {
                        Expr::Bind(i) => Target::Bind(*i),
                        Expr::Column { table: None, name } => Target::Var(name.clone()),
                        Expr::Column {
                            table: Some(rec),
                            name,
                        } if !rec.ends_with('%') => Target::Field(rec.clone(), name.clone()),
                        _ => {
                            return Err(pls(
                                363,
                                "expression cannot be used as an assignment target",
                            )
                            .into())
                        }
                    };
                    let current = if mode == ParamMode::InOut {
                        self.eval(e)?
                    } else {
                        Value::Null
                    };
                    ArgValue::Out(t, current)
                }
                (_, None) => return Err(wrong_args()),
            });
        }
        let (result, outs) = self.invoke(r, values)?;
        for (t, v) in outs {
            self.assign(&t, v)?;
        }
        Ok(r.is_function().then_some(result))
    }

    /// Runs a routine with evaluated arguments. Returns the function result and the
    /// final values of OUT arguments.
    fn invoke(
        &mut self,
        r: &Arc<Routine>,
        args: Vec<ArgValue>,
    ) -> R<(Value, Vec<(Target, Value)>)> {
        let depth = &self.host.env().depth;
        if depth.get() >= MAX_CALL_DEPTH {
            return Err(OraError::new(
                36,
                format!("maximum number of recursive SQL levels ({MAX_CALL_DEPTH}) exceeded"),
            )
            .into());
        }
        depth.set(depth.get() + 1);
        let result = self.invoke_inner(r, args);
        let depth = &self.host.env().depth;
        depth.set(depth.get() - 1);
        result
    }

    fn invoke_inner(
        &mut self,
        r: &Arc<Routine>,
        args: Vec<ArgValue>,
    ) -> R<(Value, Vec<(Target, Value)>)> {
        let mut inner = Interp::new(&mut *self.host, Vec::new());
        inner.frames.push(Frame::default());
        // Local routines of enclosing blocks stay callable, including this one for recursion.
        for f in &self.frames {
            inner.frames[0].routines.extend(f.routines.iter().cloned());
        }
        let mut outs = Vec::new();
        for (p, a) in r.params.iter().zip(args) {
            let (ty, slot) = inner.var_type(&p.ty)?;
            let (value, constant) = match a {
                ArgValue::In(v) => (v, true),
                ArgValue::Out(t, v) => {
                    outs.push((p.name.clone(), t));
                    (v, false)
                }
            };
            let slot = match slot {
                Slot::Scalar(_) => Slot::Scalar(inner.convert(ty, value)?),
                record => record,
            };
            inner.declare(
                &p.name,
                Var {
                    ty,
                    slot,
                    constant,
                    not_null: false,
                },
            );
        }
        let flow = inner.block(&r.block)?;
        let result = match (flow, &r.returns) {
            (Flow::Return(Some(v)), Some(ty)) => {
                let (ty, _) = inner.var_type(ty)?;
                inner.convert(ty, v)?
            }
            (_, Some(_)) => {
                return Err(OraError::new(6503, "PL/SQL: Function returned without value").into())
            }
            _ => Value::Null,
        };
        let mut out_values = Vec::with_capacity(outs.len());
        for (name, t) in outs {
            let v = match inner.find_var(&name).map(|v| &v.slot) {
                Some(Slot::Scalar(v)) => v.clone(),
                _ => Value::Null,
            };
            out_values.push((t, v));
        }
        Ok((result, out_values))
    }

    /// Built-in procedures. Returns false if `name` is not one.
    fn builtin(&mut self, name: &str, args: &[Arg]) -> R<bool> {
        let short = name.strip_prefix("SYS.").unwrap_or(name);
        let arg = |i: usize| args.get(i).map(|a| &a.value);
        match short {
            "RAISE_APPLICATION_ERROR" => {
                let (Some(code), Some(message)) = (arg(0), arg(1)) else {
                    return Err(pls(
                        306,
                        "wrong number or types of arguments in call to 'RAISE_APPLICATION_ERROR'",
                    )
                    .into());
                };
                let code = self.eval_int(code)?;
                let message = self.eval(message)?.to_string();
                if !(-20999..=-20000).contains(&code) {
                    return Err(OraError::new(
                        21000,
                        format!("error number argument to raise_application_error of {code} is out of range"),
                    )
                    .into());
                }
                Err(OraError::new(code.unsigned_abs() as u32, message).into())
            }
            "DBMS_OUTPUT.PUT_LINE" | "DBMS_OUTPUT.PUT" => {
                let text = match arg(0) {
                    Some(e) => {
                        let v = self.eval(e)?;
                        let env = self.host.env();
                        match v {
                            Value::Date(d) => crate::datetime::format(&d, &env.nls_date_format),
                            Value::Timestamp(d) => {
                                crate::datetime::format(&d, &env.nls_timestamp_format)
                            }
                            v => v.to_string(),
                        }
                    }
                    None => String::new(),
                };
                let mut out = self.host.env().output.borrow_mut();
                out.put(&text, short.ends_with("LINE"))?;
                Ok(true)
            }
            "DBMS_OUTPUT.NEW_LINE" => {
                self.host.env().output.borrow_mut().put("", true)?;
                Ok(true)
            }
            "DBMS_OUTPUT.ENABLE" => {
                self.host.env().output.borrow_mut().enabled = true;
                Ok(true)
            }
            "DBMS_OUTPUT.DISABLE" => {
                let mut out = self.host.env().output.borrow_mut();
                out.enabled = false;
                out.lines.clear();
                out.partial.clear();
                out.bytes = 0;
                Ok(true)
            }
            "DBMS_OUTPUT.GET_LINE" => {
                let targets: Vec<Target> = args
                    .iter()
                    .take(2)
                    .map(|a| match &a.value {
                        Expr::Bind(i) => Ok(Target::Bind(*i)),
                        Expr::Column { table: None, name } => Ok(Target::Var(name.clone())),
                        _ => Err(Exc::from(pls(
                            363,
                            "expression cannot be used as an assignment target",
                        ))),
                    })
                    .collect::<R<_>>()?;
                if targets.len() != 2 {
                    return Err(pls(
                        306,
                        "wrong number or types of arguments in call to 'GET_LINE'",
                    )
                    .into());
                }
                let line = self.host.env().output.borrow_mut().take_line();
                let status = if line.is_some() { 0 } else { 1 };
                self.assign(&targets[0], line.map_or(Value::Null, Value::varchar))?;
                self.assign(&targets[1], Value::number(status))?;
                Ok(true)
            }
            "DBMS_LOCK.SLEEP" | "DBMS_SESSION.SLEEP" => {
                for a in args {
                    self.eval(&a.value)?;
                }
                Ok(true)
            }
            _ if short.starts_with("DBMS_STATS.") => Ok(true),
            _ => Ok(false),
        }
    }
}

/// DBMS_OUTPUT's buffer for one session.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub enabled: bool,
    pub lines: VecDeque<String>,
    pub partial: String,
    /// Bytes held in the buffer, counted against its limit.
    bytes: usize,
}

impl Output {
    const LIMIT: usize = 1_000_000;

    fn put(&mut self, text: &str, end_line: bool) -> Result<(), OraError> {
        if !self.enabled {
            return Ok(());
        }
        self.bytes += text.len();
        if self.bytes > Self::LIMIT {
            return Err(OraError::new(
                20000,
                format!("ORU-10027: buffer overflow, limit of {} bytes", Self::LIMIT),
            ));
        }
        self.partial.push_str(text);
        if end_line {
            self.lines.push_back(std::mem::take(&mut self.partial));
        }
        Ok(())
    }

    /// Removes the oldest complete line, as GET_LINE does.
    fn take_line(&mut self) -> Option<String> {
        let line = self.lines.pop_front()?;
        self.bytes = self.bytes.saturating_sub(line.len());
        Some(line)
    }
}
