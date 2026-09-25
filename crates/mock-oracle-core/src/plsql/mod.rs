//! PL/SQL: anonymous blocks, stored procedures and functions.
//!
//! Blocks are parsed into the tree below by [`parse`] and run by [`exec`]. SQL inside a
//! block reuses the SQL parser and evaluator; PL/SQL variables reach SQL as an extra
//! scope that column names are resolved against last, as in Oracle.

pub(crate) mod exec;

use std::sync::Arc;

use crate::ast::{Expr, Query, Statement};
use crate::SqlType;

#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub decls: Vec<Decl>,
    pub body: Vec<Stmt>,
    pub handlers: Vec<Handler>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Decl {
    Var(VarDecl),
    /// A user-defined exception.
    Exception(String),
    /// `PRAGMA EXCEPTION_INIT(name, -code)`.
    ExceptionInit {
        name: String,
        code: u32,
    },
    Cursor(CursorDecl),
    /// A procedure or function local to the block.
    Routine(Arc<Routine>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct VarDecl {
    pub name: String,
    pub ty: TypeRef,
    pub constant: bool,
    pub not_null: bool,
    pub default: Option<Expr>,
}

/// A declared type, resolved when the declaration runs.
#[derive(Debug, Clone, PartialEq)]
pub enum TypeRef {
    Sql(SqlType),
    Boolean,
    /// PLS_INTEGER, BINARY_INTEGER and friends: whole numbers.
    Integer,
    /// `table.column%TYPE`, or `variable%TYPE` when `table` is `None`.
    TypeOf {
        table: Option<String>,
        name: String,
    },
    /// `table%ROWTYPE` or `cursor%ROWTYPE`.
    RowType(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CursorDecl {
    pub name: String,
    pub params: Vec<Param>,
    pub query: Query,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Handler {
    /// Exception names, or `OTHERS`.
    pub exceptions: Vec<String>,
    pub body: Vec<Stmt>,
}

/// Something a value can be assigned to.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    Var(String),
    /// `record.field`
    Field(String, String),
    /// A bind variable of the block, by position.
    Bind(usize),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Arg {
    /// Set for named notation, `name => value`.
    pub name: Option<String>,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CursorSource {
    Query(Query),
    Named { name: String, args: Vec<Expr> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Null,
    Assign {
        target: Target,
        value: Expr,
    },
    If {
        branches: Vec<(Expr, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    /// The CASE statement. Without ELSE, no match raises CASE_NOT_FOUND.
    Case {
        operand: Option<Expr>,
        whens: Vec<(Expr, Vec<Stmt>)>,
        else_: Option<Vec<Stmt>>,
    },
    Loop {
        label: Option<String>,
        body: Vec<Stmt>,
    },
    While {
        label: Option<String>,
        cond: Expr,
        body: Vec<Stmt>,
    },
    ForRange {
        label: Option<String>,
        var: String,
        reverse: bool,
        low: Expr,
        high: Expr,
        body: Vec<Stmt>,
    },
    ForCursor {
        label: Option<String>,
        var: String,
        source: CursorSource,
        body: Vec<Stmt>,
    },
    Exit {
        label: Option<String>,
        when: Option<Expr>,
    },
    Continue {
        label: Option<String>,
        when: Option<Expr>,
    },
    Return(Option<Expr>),
    /// `RAISE name`, or `RAISE` alone to re-raise inside a handler.
    Raise(Option<String>),
    /// A SQL statement. `into` holds the targets of `SELECT ... INTO`.
    Sql {
        stmt: Statement,
        into: Vec<Target>,
    },
    /// A procedure call, including built-ins such as DBMS_OUTPUT.PUT_LINE.
    Call {
        name: String,
        args: Vec<Arg>,
    },
    ExecuteImmediate {
        sql: Expr,
        into: Vec<Target>,
        using: Vec<Expr>,
    },
    Open {
        cursor: String,
        args: Vec<Expr>,
    },
    Fetch {
        cursor: String,
        into: Vec<Target>,
    },
    Close {
        cursor: String,
    },
    Block(Block),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamMode {
    In,
    Out,
    InOut,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub name: String,
    pub mode: ParamMode,
    pub ty: TypeRef,
    pub default: Option<Expr>,
}

/// A stored or local procedure or function.
#[derive(Debug, Clone, PartialEq)]
pub struct Routine {
    pub name: String,
    pub params: Vec<Param>,
    /// The return type of a function; `None` for a procedure.
    pub returns: Option<TypeRef>,
    pub block: Block,
}

impl Routine {
    pub fn is_function(&self) -> bool {
        self.returns.is_some()
    }
}
