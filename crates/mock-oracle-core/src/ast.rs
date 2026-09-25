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
    Commit,
    Rollback,
    /// `ALTER SESSION SET name = value`.
    AlterSession {
        name: String,
        value: String,
    },
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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Delete {
    pub table: String,
    pub alias: Option<String>,
    pub where_: Option<Expr>,
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

#[derive(Debug, Clone, PartialEq)]
pub struct TableConstraint {
    pub name: Option<String>,
    pub kind: ConstraintKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ConstraintKind {
    PrimaryKey(Vec<String>),
    Unique(Vec<String>),
    Check(Expr),
    /// Parsed and kept, but not enforced yet.
    ForeignKey {
        columns: Vec<String>,
        table: String,
        ref_columns: Vec<String>,
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
}
