use crate::Value;

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Select(Select),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Select {
    pub items: Vec<SelectItem>,
    pub from: TableRef,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    pub expr: Expr,
    /// The column name Oracle reports: the alias, or the expression text
    /// uppercased with whitespace removed.
    pub name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    Dual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Concat,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Literal(Value),
    /// A bind variable, by its position among all bind occurrences in the statement.
    Bind(usize),
    Column(String),
    /// `*` in a select list; expands to every column of the table.
    Star,
    Neg(Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
}
