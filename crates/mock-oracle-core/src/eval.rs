//! Evaluates parsed statements.

use std::str::FromStr;

use bigdecimal::{BigDecimal, Zero};

use crate::ast::{BinaryOp, Expr, Select, SelectItem, Statement, TableRef};
use crate::{Column, OraError, QueryResult, SqlType, Value};

pub fn execute(stmt: &Statement, binds: &[Value]) -> Result<QueryResult, OraError> {
    match stmt {
        Statement::Select(select) => execute_select(select, binds),
    }
}

fn execute_select(select: &Select, binds: &[Value]) -> Result<QueryResult, OraError> {
    let TableRef::Dual = select.from;
    // DUAL has one column, DUMMY, so `*` expands to it.
    let items: Vec<SelectItem> = select
        .items
        .iter()
        .map(|item| match item.expr {
            Expr::Star => SelectItem {
                expr: Expr::Column("DUMMY".into()),
                name: "DUMMY".into(),
            },
            _ => item.clone(),
        })
        .collect();
    let row = items
        .iter()
        .map(|item| eval(&item.expr, binds))
        .collect::<Result<Vec<_>, _>>()?;
    let columns = items
        .iter()
        .zip(&row)
        .map(|(item, value)| Column {
            name: item.name.clone(),
            sql_type: type_of(&item.expr, value),
        })
        .collect();
    Ok(QueryResult {
        columns,
        rows: vec![row],
        rows_affected: 0,
        is_query: true,
    })
}

/// The column type Oracle would describe for an expression.
fn type_of(expr: &Expr, value: &Value) -> SqlType {
    match (expr, value) {
        (Expr::Binary(BinaryOp::Concat, ..), v) => SqlType::Varchar2(byte_len(v).max(1)),
        (Expr::Binary(..) | Expr::Neg(_), _) => SqlType::Number,
        (_, Value::Number(_)) => SqlType::Number,
        (_, v) => SqlType::Varchar2(byte_len(v)),
    }
}

fn byte_len(v: &Value) -> u32 {
    match v {
        Value::Varchar2(s) => s.len() as u32,
        _ => 0,
    }
}

fn eval(expr: &Expr, binds: &[Value]) -> Result<Value, OraError> {
    match expr {
        Expr::Literal(v) => Ok(v.clone()),
        Expr::Bind(i) => binds
            .get(*i)
            .cloned()
            .ok_or_else(|| OraError::new(1008, "not all variables bound")),
        Expr::Column(name) if name == "DUMMY" => Ok(Value::varchar("X")),
        Expr::Column(name) => Err(OraError::new(
            904,
            format!("\"{name}\": invalid identifier"),
        )),
        Expr::Star => Err(OraError::new(936, "missing expression")),
        Expr::Neg(inner) => match to_number(eval(inner, binds)?)? {
            None => Ok(Value::Null),
            Some(n) => Ok(Value::Number(-n)),
        },
        Expr::Binary(BinaryOp::Concat, l, r) => {
            let l = eval(l, binds)?;
            let r = eval(r, binds)?;
            Ok(Value::varchar(format!("{l}{r}")))
        }
        Expr::Binary(op, l, r) => {
            let (Some(l), Some(r)) = (to_number(eval(l, binds)?)?, to_number(eval(r, binds)?)?)
            else {
                return Ok(Value::Null);
            };
            let n = match op {
                BinaryOp::Add => l + r,
                BinaryOp::Sub => l - r,
                BinaryOp::Mul => l * r,
                BinaryOp::Div => {
                    if r.is_zero() {
                        return Err(OraError::new(1476, "divisor is equal to zero"));
                    }
                    // Oracle NUMBER keeps up to 40 significant digits.
                    (l / r).with_prec(40)
                }
                BinaryOp::Concat => unreachable!(),
            };
            Ok(Value::Number(n))
        }
    }
}

/// Implicit conversion to NUMBER, as Oracle does for arithmetic on strings.
fn to_number(v: Value) -> Result<Option<BigDecimal>, OraError> {
    match v {
        Value::Null => Ok(None),
        Value::Number(n) => Ok(Some(n)),
        Value::Varchar2(s) => BigDecimal::from_str(s.trim())
            .map(Some)
            .map_err(|_| OraError::new(1722, "invalid number")),
    }
}
