use std::sync::Arc;

use crate::plsql::{Block, Routine};
use crate::{SqlType, Value};

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Query(Query),
    Insert(Insert),
    Update(Update),
    Delete(Delete),
    CreateTable(CreateTable),
    DropTable {
        name: String,
        /// `CASCADE CONSTRAINTS`: also drop the foreign keys that reference the table.
        cascade_constraints: bool,
    },
    AlterTable {
        name: String,
        action: AlterTableAction,
    },
    /// Only unique indexes have an effect: they enforce uniqueness like a UNIQUE constraint.
    CreateIndex {
        name: String,
        table: String,
        columns: Vec<String>,
        unique: bool,
    },
    DropIndex {
        name: String,
    },
    Truncate {
        name: String,
    },
    CreateSequence {
        name: String,
        options: Vec<SequenceOption>,
    },
    AlterSequence {
        name: String,
        options: Vec<SequenceOption>,
    },
    DropSequence {
        name: String,
    },
    /// `CREATE [OR REPLACE] PROCEDURE` or `FUNCTION`.
    CreateRoutine {
        routine: Arc<Routine>,
        or_replace: bool,
    },
    DropRoutine {
        name: String,
        function: bool,
    },
    /// An anonymous PL/SQL block (`BEGIN`, `DECLARE` or `CALL`).
    Block(Box<Block>),
    Commit,
    Rollback,
    /// `ALTER SESSION SET name = value`.
    AlterSession {
        name: String,
        value: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum SequenceOption {
    StartWith(i128),
    IncrementBy(i128),
    /// `MINVALUE n`, or `NOMINVALUE` as `None`.
    MinValue(Option<i128>),
    MaxValue(Option<i128>),
    Cycle(bool),
    /// `RESTART [START WITH n]` in ALTER SEQUENCE.
    Restart(Option<i128>),
}

/// `RETURNING exprs INTO targets` on INSERT, UPDATE and DELETE.
#[derive(Debug, Clone, PartialEq)]
pub struct Returning {
    pub exprs: Vec<Expr>,
    /// Bind variables, or PL/SQL variables (as column references) inside a block.
    pub into: Vec<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub body: SetExpr,
    pub order_by: Vec<OrderItem>,
    pub offset: Option<Expr>,
    pub fetch: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SetExpr {
    Select(Box<Select>),
    SetOp {
        op: SetOp,
        left: Box<SetExpr>,
        right: Box<SetExpr>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    Union,
    UnionAll,
    Intersect,
    Minus,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub distinct: bool,
    pub items: Vec<SelectItem>,
    pub from: Vec<FromItem>,
    pub where_: Option<Expr>,
    pub group_by: Vec<Expr>,
    pub having: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SelectItem {
    Expr {
        expr: Expr,
        /// The column name Oracle reports: the alias, or the expression text
        /// uppercased with whitespace removed.
        name: String,
    },
    /// `*`
    Star,
    /// `t.*`
    QualifiedStar(String),
}

/// A table in FROM, with the tables joined to it.
#[derive(Debug, Clone, PartialEq)]
pub struct FromItem {
    pub factor: TableFactor,
    pub joins: Vec<Join>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableFactor {
    Table {
        name: String,
        alias: Option<String>,
    },
    Subquery {
        query: Box<Query>,
        alias: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Join {
    pub kind: JoinKind,
    pub factor: TableFactor,
    pub on: Option<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    pub expr: Expr,
    pub desc: bool,
    /// Explicit NULLS FIRST (true) or NULLS LAST (false). Oracle's default puts
    /// NULLs last when ascending and first when descending.
    pub nulls_first: Option<bool>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    pub columns: Option<Vec<String>>,
    pub source: InsertSource,
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum InsertSource {
    Values(Vec<Expr>),
    Query(Box<Query>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Update {
    pub table: String,
    pub alias: Option<String>,
    pub assignments: Vec<(String, Expr)>,
    pub where_: Option<Expr>,
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub alias: Option<String>,
    pub where_: Option<Expr>,
    pub returning: Option<Returning>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    pub name: String,
    pub columns: Vec<ColumnDef>,
    pub constraints: Vec<TableConstraint>,
    /// `CREATE TABLE ... AS SELECT`: columns and rows come from the query.
    pub as_query: Option<Box<Query>>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    pub name: String,
    pub sql_type: SqlType,
    pub default: Option<Expr>,
    pub not_null: bool,
    /// An identity column (`GENERATED ... AS IDENTITY`) gets 1, 2, 3, ... when no value is given.
    pub identity: bool,
}

/// The constraint forms of `ALTER TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub enum AlterTableAction {
    /// `ADD CONSTRAINT ...` or `ADD (constraint, ...)`.
    AddConstraints(Vec<TableConstraint>),
    /// `DROP CONSTRAINT name`, or `DROP PRIMARY KEY` when `name` is `None`.
    DropConstraint { name: Option<String>, cascade: bool },
    /// `ENABLE`/`DISABLE CONSTRAINT name`, or `MODIFY CONSTRAINT name ENABLE`/`DISABLE`;
    /// `name` is `None` for `ENABLE`/`DISABLE PRIMARY KEY`.
    SetConstraint {
        name: Option<String>,
        enabled: bool,
        /// `ENABLE NOVALIDATE` skips checking the rows already in the table.
        validate: bool,
        /// `DISABLE ... CASCADE` also disables foreign keys that depend on a key.
        cascade: bool,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct TableConstraint {
    pub name: Option<String>,
    pub kind: ConstraintKind,
    /// `DISABLE` creates the constraint without enforcing it.
    pub enabled: bool,
    /// `ENABLE NOVALIDATE` enforces it for new changes only, not for existing rows.
    pub validate: bool,
}

/// What deleting a parent row does to the child rows that reference it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnDelete {
    /// The delete fails while child rows exist (ORA-02292).
    Restrict,
    Cascade,
    SetNull,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    Check(Expr),
    /// `ref_columns` is empty when the statement names only the table: the parent's
    /// primary key is meant.
    ForeignKey {
        columns: Vec<String>,
        table: String,
        ref_columns: Vec<String>,
        on_delete: OnDelete,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Concat,
    Eq,
    NotEq,
    Lt,
    LtEq,
    Gt,
    GtEq,
    And,
    Or,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A bind variable, by its position among all bind occurrences in the statement.
    Bind(usize),
    Column {
        table: Option<String>,
        name: String,
    },
    Neg(Box<Expr>),
    Not(Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    IsNull {
        expr: Box<Expr>,
        negated: bool,
    },
    Between {
        expr: Box<Expr>,
        low: Box<Expr>,
        high: Box<Expr>,
        negated: bool,
    },
    InList {
        expr: Box<Expr>,
        list: Vec<Expr>,
        negated: bool,
    },
    InSubquery {
        expr: Box<Expr>,
        query: Box<Query>,
        negated: bool,
    },
    Exists(Box<Query>),
    Like {
        expr: Box<Expr>,
        pattern: Box<Expr>,
        escape: Option<Box<Expr>>,
        negated: bool,
    },
    Case {
        operand: Option<Box<Expr>>,
        whens: Vec<(Expr, Expr)>,
        else_: Option<Box<Expr>>,
    },
    Function {
        name: String,
        args: Vec<Expr>,
        distinct: bool,
    },
    /// `COUNT(*)`
    CountStar,
    Subquery(Box<Query>),
    RowNum,
    /// `seq.NEXTVAL` or `seq.CURRVAL`.
    Sequence {
        name: String,
        next: bool,
    },
}
