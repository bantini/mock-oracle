//! Recursive-descent parser for the supported subset of Oracle SQL.

use crate::ast::*;
use crate::lexer::{tokenize, Tok, Token};
use crate::{datetime, OraError, SqlType, Value};

/// A parsed statement and the number of bind placeholders it contains.
#[derive(Debug, Clone)]
pub struct Parsed {
    pub stmt: Statement,
    pub binds: usize,
}

pub fn parse(sql: &str) -> Result<Parsed, OraError> {
    let tokens = tokenize(sql)?;
    let mut p = Parser {
        sql,
        tokens,
        pos: 0,
        binds: 0,
    };
    let stmt = p.statement()?;
    match p.peek() {
        None => Ok(Parsed {
            stmt,
            binds: p.binds,
        }),
        Some(Tok::Symbol(";")) => Err(OraError::new(911, "invalid character")),
        Some(_) => Err(OraError::new(933, "SQL command not properly ended")),
    }
}

/// Words that end an expression or clause, so they cannot be read as an alias.
const RESERVED: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "UNION",
    "MINUS",
    "INTERSECT",
    "EXCEPT",
    "JOIN",
    "INNER",
    "LEFT",
    "RIGHT",
    "FULL",
    "CROSS",
    "ON",
    "CONNECT",
    "START",
    "FETCH",
    "OFFSET",
    "FOR",
    "AND",
    "OR",
    "NOT",
    "IS",
    "IN",
    "LIKE",
    "BETWEEN",
    "AS",
    "SET",
    "VALUES",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "ASC",
    "DESC",
    "NULLS",
    "WITH",
    "SELECT",
    "USING",
    "NATURAL",
    "OUTER",
    "RETURNING",
    "DISTINCT",
];

struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    binds: usize,
}

fn missing_keyword() -> OraError {
    OraError::new(905, "missing keyword")
}

fn missing_paren() -> OraError {
    OraError::new(907, "missing right parenthesis")
}

fn missing_left_paren() -> OraError {
    OraError::new(906, "missing left parenthesis")
}

fn unsupported(what: &str) -> OraError {
    OraError::new(3001, format!("unimplemented feature: {what}"))
}

impl Parser<'_> {
    // ---- token helpers ----

    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn peek_at(&self, n: usize) -> Option<&Tok> {
        self.tokens.get(self.pos + n).map(|t| &t.tok)
    }

    fn is_kw(&self, kw: &str) -> bool {
        self.is_kw_at(0, kw)
    }

    fn is_kw_at(&self, n: usize, kw: &str) -> bool {
        matches!(self.peek_at(n), Some(Tok::Ident(k)) if k == kw)
    }

    fn is_sym(&self, sym: &str) -> bool {
        self.is_sym_at(0, sym)
    }

    fn is_sym_at(&self, n: usize, sym: &str) -> bool {
        matches!(self.peek_at(n), Some(Tok::Symbol(s)) if *s == sym)
    }

    fn is_ident_at(&self, n: usize) -> bool {
        matches!(
            self.peek_at(n),
            Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_))
        )
    }

    fn advance(&mut self) -> Option<Tok> {
        let t = self.tokens.get(self.pos).map(|t| t.tok.clone());
        self.pos += 1;
        t
    }

    fn skip_rest(&mut self) {
        self.pos = self.tokens.len();
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        let yes = self.is_kw(kw);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        let yes = self.is_sym(sym);
        if yes {
            self.pos += 1;
        }
        yes
    }

    fn expect_kw(&mut self, kw: &str) -> Result<(), OraError> {
        if self.eat_kw(kw) {
            Ok(())
        } else {
            Err(missing_keyword())
        }
    }

    fn expect_sym(&mut self, sym: &str) -> Result<(), OraError> {
        if self.eat_sym(sym) {
            Ok(())
        } else if sym == ")" {
            Err(missing_paren())
        } else if sym == "(" {
            Err(missing_left_paren())
        } else {
            Err(OraError::new(933, "SQL command not properly ended"))
        }
    }

    /// An identifier, quoted or not. Unquoted ones are already uppercase.
    fn ident(&mut self) -> Result<String, OraError> {
        match self.peek().cloned() {
            Some(Tok::Ident(n)) | Some(Tok::QuotedIdent(n)) => {
                self.pos += 1;
                Ok(n)
            }
            _ => Err(OraError::new(904, "invalid identifier")),
        }
    }

    /// An object name, dropping any schema prefix (`HR.EMPLOYEES` becomes `EMPLOYEES`).
    fn object_name(&mut self) -> Result<String, OraError> {
        let invalid = || OraError::new(903, "invalid table name");
        let first = self.ident().map_err(|_| invalid())?;
        if self.eat_sym(".") {
            return self.ident().map_err(|_| invalid());
        }
        Ok(first)
    }

    fn optional_alias(&mut self) -> Result<Option<String>, OraError> {
        if self.eat_kw("AS") {
            return self.ident().map(Some);
        }
        match self.peek() {
            Some(Tok::QuotedIdent(_)) => self.ident().map(Some),
            Some(Tok::Ident(k)) if !RESERVED.contains(&k.as_str()) => self.ident().map(Some),
            _ => Ok(None),
        }
    }

    fn integer(&mut self) -> Result<i64, OraError> {
        let neg = self.eat_sym("-");
        match self.advance() {
            Some(Tok::Number(n)) => n
                .parse::<i64>()
                .map(|v| if neg { -v } else { v })
                .map_err(|_| OraError::new(1722, "invalid number")),
            _ => Err(OraError::new(1722, "invalid number")),
        }
    }

    fn ident_list(&mut self) -> Result<Vec<String>, OraError> {
        self.expect_sym("(")?;
        let mut out = vec![self.ident()?];
        while self.eat_sym(",") {
            out.push(self.ident()?);
        }
        self.expect_sym(")")?;
        Ok(out)
    }

    /// Skips a balanced parenthesised group, if one starts here.
    fn skip_parens(&mut self) -> Result<(), OraError> {
        if !self.eat_sym("(") {
            return Ok(());
        }
        let mut depth = 1;
        while depth > 0 {
            match self.advance() {
                Some(Tok::Symbol("(")) => depth += 1,
                Some(Tok::Symbol(")")) => depth -= 1,
                None => return Err(missing_paren()),
                _ => {}
            }
        }
        Ok(())
    }

    // ---- statements ----

    fn statement(&mut self) -> Result<Statement, OraError> {
        if self.is_sym("(") {
            return Ok(Statement::Query(self.query()?));
        }
        let Some(Tok::Ident(first)) = self.peek().cloned() else {
            return Err(OraError::new(900, "invalid SQL statement"));
        };
        match first.as_str() {
            "SELECT" => Ok(Statement::Query(self.query()?)),
            "WITH" => Err(unsupported("WITH clause")),
            "INSERT" => self.insert(),
            "UPDATE" => self.update(),
            "DELETE" => self.delete(),
            "CREATE" => self.create(),
            "DROP" => self.drop(),
            "TRUNCATE" => {
                self.advance();
                self.expect_kw("TABLE")?;
                let name = self.object_name()?;
                self.skip_rest(); // DROP STORAGE, REUSE STORAGE
                Ok(Statement::Truncate { name })
            }
            "COMMIT" => {
                self.advance();
                self.eat_kw("WORK");
                Ok(Statement::Commit)
            }
            "ROLLBACK" => {
                self.advance();
                self.eat_kw("WORK");
                if self.is_kw("TO") {
                    return Err(unsupported("savepoints"));
                }
                Ok(Statement::Rollback)
            }
            "ALTER" if self.is_kw_at(1, "SESSION") => {
                self.pos += 2;
                self.expect_kw("SET")?;
                let name = self.ident()?;
                self.expect_sym("=")?;
                let value = match self.advance() {
                    Some(Tok::Str(s))
                    | Some(Tok::Ident(s))
                    | Some(Tok::Number(s))
                    | Some(Tok::QuotedIdent(s)) => s,
                    _ => return Err(OraError::missing_expression()),
                };
                // Further settings in the same statement are ignored.
                self.skip_rest();
                Ok(Statement::AlterSession { name, value })
            }
            _ => Err(OraError::new(900, "invalid SQL statement")),
        }
    }

    fn insert(&mut self) -> Result<Statement, OraError> {
        self.advance();
        self.expect_kw("INTO")?;
        let table = self.object_name()?;
        self.optional_alias()?;
        let columns = if self.is_sym("(") && !self.is_kw_at(1, "SELECT") {
            Some(self.ident_list()?)
        } else {
            None
        };
        let source = if self.eat_kw("VALUES") {
            self.expect_sym("(")?;
            let mut values = vec![self.expr()?];
            while self.eat_sym(",") {
                values.push(self.expr()?);
            }
            self.expect_sym(")")?;
            InsertSource::Values(values)
        } else if self.is_kw("SELECT") || self.is_sym("(") {
            InsertSource::Query(Box::new(self.query()?))
        } else {
            return Err(OraError::new(926, "missing VALUES keyword"));
        };
        if self.is_kw("RETURNING") {
            return Err(unsupported("RETURNING INTO"));
        }
        Ok(Statement::Insert(Insert {
            table,
            columns,
            source,
        }))
    }

    fn update(&mut self) -> Result<Statement, OraError> {
        self.advance();
        let table = self.object_name()?;
        let alias = self.optional_alias()?;
        self.expect_kw("SET")?;
        let mut assignments = Vec::new();
        loop {
            if self.is_sym("(") {
                return Err(unsupported("multi-column UPDATE SET"));
            }
            let mut col = self.ident()?;
            if self.eat_sym(".") {
                col = self.ident()?;
            }
            if !self.eat_sym("=") {
                return Err(OraError::new(927, "missing equal sign"));
            }
            assignments.push((col, self.expr()?));
            if !self.eat_sym(",") {
                break;
            }
        }
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        if self.is_kw("RETURNING") {
            return Err(unsupported("RETURNING INTO"));
        }
        Ok(Statement::Update(Update {
            table,
            alias,
            assignments,
            where_,
        }))
    }

    fn delete(&mut self) -> Result<Statement, OraError> {
        self.advance();
        self.eat_kw("FROM");
        let table = self.object_name()?;
        let alias = self.optional_alias()?;
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        if self.is_kw("RETURNING") {
            return Err(unsupported("RETURNING INTO"));
        }
        Ok(Statement::Delete(Delete {
            table,
            alias,
            where_,
        }))
    }

    fn drop(&mut self) -> Result<Statement, OraError> {
        self.advance();
        if self.eat_kw("TABLE") {
            let name = self.object_name()?;
            self.skip_rest(); // CASCADE CONSTRAINTS, PURGE
            return Ok(Statement::DropTable { name });
        }
        if self.eat_kw("INDEX") {
            let name = self.object_name()?;
            self.skip_rest();
            return Ok(Statement::DropIndex { name });
        }
        Err(OraError::new(950, "invalid DROP option"))
    }

    fn create(&mut self) -> Result<Statement, OraError> {
        self.advance();
        if self.eat_kw("OR") {
            self.expect_kw("REPLACE")?;
        }
        // Global temporary tables are created as normal tables.
        if self.eat_kw("GLOBAL") {
            self.expect_kw("TEMPORARY")?;
        }
        let unique = self.eat_kw("UNIQUE");
        self.eat_kw("BITMAP");
        if self.eat_kw("INDEX") {
            return self.create_index(unique);
        }
        if self.is_kw("SEQUENCE") {
            return Err(unsupported("sequences"));
        }
        if !self.eat_kw("TABLE") {
            return Err(OraError::new(901, "invalid CREATE command"));
        }
        let name = self.object_name()?;
        let mut columns = Vec::new();
        let mut constraints = Vec::new();
        if self.eat_sym("(") {
            loop {
                if ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
                    .iter()
                    .any(|k| self.is_kw(k))
                {
                    constraints.push(self.table_constraint()?);
                } else {
                    let (col, mut col_constraints) = self.column_def()?;
                    columns.push(col);
                    constraints.append(&mut col_constraints);
                }
                if !self.eat_sym(",") {
                    break;
                }
            }
            self.expect_sym(")")?;
        }
        // Skip physical attributes (TABLESPACE, STORAGE, ON COMMIT ... ROWS, ...) up to AS SELECT.
        let mut as_query = None;
        while self.peek().is_some() {
            if self.is_kw("AS") && (self.is_kw_at(1, "SELECT") || self.is_sym_at(1, "(")) {
                self.advance();
                as_query = Some(Box::new(self.query()?));
                break;
            }
            self.advance();
        }
        if columns.is_empty() && as_query.is_none() {
            return Err(missing_left_paren());
        }
        Ok(Statement::CreateTable(CreateTable {
            name,
            columns,
            constraints,
            as_query,
        }))
    }

    fn create_index(&mut self, unique: bool) -> Result<Statement, OraError> {
        let name = self.object_name()?;
        self.expect_kw("ON")?;
        let table = self.object_name()?;
        self.expect_sym("(")?;
        let mut columns = Vec::new();
        loop {
            columns.push(self.ident()?);
            if !self.eat_kw("ASC") {
                self.eat_kw("DESC");
            }
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        self.skip_rest();
        Ok(Statement::CreateIndex {
            name,
            table,
            columns,
            unique,
        })
    }

    fn column_def(&mut self) -> Result<(ColumnDef, Vec<TableConstraint>), OraError> {
        let name = self.ident()?;
        let sql_type = self.data_type()?;
        let mut col = ColumnDef {
            name: name.clone(),
            sql_type,
            default: None,
            not_null: false,
            identity: false,
        };
        let mut constraints = Vec::new();
        loop {
            if self.eat_kw("DEFAULT") {
                if self.eat_kw("ON") {
                    self.expect_kw("NULL")?;
                }
                col.default = Some(self.expr()?);
                continue;
            }
            if self.eat_kw("GENERATED") {
                if !self.eat_kw("ALWAYS") {
                    self.expect_kw("BY")?;
                    self.expect_kw("DEFAULT")?;
                    if self.eat_kw("ON") {
                        self.expect_kw("NULL")?;
                    }
                }
                self.expect_kw("AS")?;
                self.expect_kw("IDENTITY")?;
                self.skip_parens()?;
                col.identity = true;
                col.not_null = true;
                continue;
            }
            let cname = if self.eat_kw("CONSTRAINT") {
                Some(self.ident()?)
            } else {
                None
            };
            if self.eat_kw("NOT") {
                self.expect_kw("NULL")?;
                col.not_null = true;
            } else if self.eat_kw("NULL") {
            } else if self.eat_kw("PRIMARY") {
                self.expect_kw("KEY")?;
                constraints.push(TableConstraint {
                    name: cname,
                    kind: ConstraintKind::PrimaryKey(vec![name.clone()]),
                });
            } else if self.eat_kw("UNIQUE") {
                constraints.push(TableConstraint {
                    name: cname,
                    kind: ConstraintKind::Unique(vec![name.clone()]),
                });
            } else if self.eat_kw("CHECK") {
                self.expect_sym("(")?;
                let e = self.expr()?;
                self.expect_sym(")")?;
                constraints.push(TableConstraint {
                    name: cname,
                    kind: ConstraintKind::Check(e),
                });
            } else if self.eat_kw("REFERENCES") {
                let (table, ref_columns) = self.references()?;
                constraints.push(TableConstraint {
                    name: cname,
                    kind: ConstraintKind::ForeignKey {
                        columns: vec![name.clone()],
                        table,
                        ref_columns,
                    },
                });
            } else if cname.is_some() {
                return Err(OraError::new(
                    2253,
                    "constraint specification not allowed here",
                ));
            } else {
                break;
            }
            if !self.eat_kw("ENABLE") {
                self.eat_kw("DISABLE");
            }
        }
        Ok((col, constraints))
    }

    fn references(&mut self) -> Result<(String, Vec<String>), OraError> {
        let table = self.object_name()?;
        let cols = if self.is_sym("(") {
            self.ident_list()?
        } else {
            Vec::new()
        };
        if self.eat_kw("ON") {
            self.expect_kw("DELETE")?;
            if !self.eat_kw("CASCADE") {
                self.expect_kw("SET")?;
                self.expect_kw("NULL")?;
            }
        }
        Ok((table, cols))
    }

    fn table_constraint(&mut self) -> Result<TableConstraint, OraError> {
        let name = if self.eat_kw("CONSTRAINT") {
            Some(self.ident()?)
        } else {
            None
        };
        let kind = if self.eat_kw("PRIMARY") {
            self.expect_kw("KEY")?;
            ConstraintKind::PrimaryKey(self.ident_list()?)
        } else if self.eat_kw("UNIQUE") {
            ConstraintKind::Unique(self.ident_list()?)
        } else if self.eat_kw("CHECK") {
            self.expect_sym("(")?;
            let e = self.expr()?;
            self.expect_sym(")")?;
            ConstraintKind::Check(e)
        } else if self.eat_kw("FOREIGN") {
            self.expect_kw("KEY")?;
            let columns = self.ident_list()?;
            self.expect_kw("REFERENCES")?;
            let (table, ref_columns) = self.references()?;
            ConstraintKind::ForeignKey {
                columns,
                table,
                ref_columns,
            }
        } else {
            return Err(OraError::new(
                2253,
                "constraint specification not allowed here",
            ));
        };
        if !self.eat_kw("ENABLE") {
            self.eat_kw("DISABLE");
        }
        Ok(TableConstraint { name, kind })
    }

    fn length(&mut self) -> Result<Option<u32>, OraError> {
        if !self.eat_sym("(") {
            return Ok(None);
        }
        let n = self.integer()?;
        if !self.eat_kw("BYTE") {
            self.eat_kw("CHAR");
        }
        self.expect_sym(")")?;
        if n <= 0 {
            return Err(OraError::new(1724, "zero-length columns are not allowed"));
        }
        Ok(Some(n as u32))
    }

    fn data_type(&mut self) -> Result<SqlType, OraError> {
        let name = match self.peek().cloned() {
            Some(Tok::Ident(n)) => {
                self.pos += 1;
                n
            }
            _ => return Err(OraError::new(902, "invalid datatype")),
        };
        Ok(match name.as_str() {
            "NUMBER" | "NUMERIC" | "DECIMAL" | "DEC" => {
                if self.eat_sym("(") {
                    let precision = if self.eat_sym("*") {
                        38
                    } else {
                        self.integer()?
                    };
                    if !(1..=38).contains(&precision) {
                        return Err(OraError::new(
                            1727,
                            "numeric precision specifier is out of range (1 to 38)",
                        ));
                    }
                    let scale = if self.eat_sym(",") {
                        self.integer()?
                    } else {
                        0
                    };
                    if !(-84..=127).contains(&scale) {
                        return Err(OraError::new(
                            1728,
                            "numeric scale specifier is out of range (-84 to 127)",
                        ));
                    }
                    self.expect_sym(")")?;
                    SqlType::Number {
                        precision: precision as u8,
                        scale: scale as i8,
                    }
                } else if name == "NUMBER" {
                    SqlType::NUMBER
                } else {
                    SqlType::Number {
                        precision: 38,
                        scale: 0,
                    }
                }
            }
            "INTEGER" | "INT" | "SMALLINT" => SqlType::Number {
                precision: 38,
                scale: 0,
            },
            "FLOAT" | "REAL" | "BINARY_DOUBLE" | "BINARY_FLOAT" => {
                self.skip_parens()?;
                SqlType::NUMBER
            }
            "DOUBLE" => {
                self.expect_kw("PRECISION")?;
                SqlType::NUMBER
            }
            "VARCHAR2" | "VARCHAR" | "NVARCHAR2" => {
                let Some(n) = self.length()? else {
                    return Err(missing_left_paren());
                };
                if n > 32767 {
                    return Err(OraError::new(
                        910,
                        "specified length too long for its datatype",
                    ));
                }
                SqlType::Varchar2(n)
            }
            "CHAR" | "NCHAR" | "CHARACTER" => {
                if self.eat_kw("VARYING") {
                    let Some(n) = self.length()? else {
                        return Err(missing_left_paren());
                    };
                    SqlType::Varchar2(n)
                } else {
                    let n = self.length()?.unwrap_or(1);
                    if n > 2000 {
                        return Err(OraError::new(
                            910,
                            "specified length too long for its datatype",
                        ));
                    }
                    SqlType::Char(n)
                }
            }
            // LOBs are modelled as long strings for now.
            "CLOB" | "NCLOB" | "LONG" => SqlType::Varchar2(32767),
            "DATE" => SqlType::Date,
            "TIMESTAMP" => {
                let p = if self.eat_sym("(") {
                    let p = self.integer()?;
                    self.expect_sym(")")?;
                    if !(0..=9).contains(&p) {
                        return Err(OraError::new(
                            30088,
                            "datetime/interval precision is out of range",
                        ));
                    }
                    p as u8
                } else {
                    6
                };
                if self.eat_kw("WITH") {
                    self.eat_kw("LOCAL");
                    self.expect_kw("TIME")?;
                    self.expect_kw("ZONE")?;
                }
                SqlType::Timestamp(p)
            }
            _ => return Err(OraError::new(902, "invalid datatype")),
        })
    }

    // ---- queries ----

    fn query(&mut self) -> Result<Query, OraError> {
        let body = self.set_expr()?;
        let mut order_by = Vec::new();
        if self.eat_kw("ORDER") {
            self.eat_kw("SIBLINGS");
            self.expect_kw("BY")?;
            loop {
                let expr = self.expr()?;
                let desc = self.eat_kw("DESC");
                if !desc {
                    self.eat_kw("ASC");
                }
                let nulls_first = if self.eat_kw("NULLS") {
                    if self.eat_kw("FIRST") {
                        Some(true)
                    } else {
                        self.expect_kw("LAST")?;
                        Some(false)
                    }
                } else {
                    None
                };
                order_by.push(OrderItem {
                    expr,
                    desc,
                    nulls_first,
                });
                if !self.eat_sym(",") {
                    break;
                }
            }
        }
        let mut offset = None;
        let mut fetch = None;
        if self.eat_kw("OFFSET") {
            offset = Some(self.additive()?);
            if !self.eat_kw("ROWS") {
                self.expect_kw("ROW")?;
            }
        }
        if self.eat_kw("FETCH") {
            if !self.eat_kw("FIRST") {
                self.expect_kw("NEXT")?;
            }
            fetch = Some(if self.is_kw("ROW") || self.is_kw("ROWS") {
                Expr::Literal(Value::number(1))
            } else {
                self.additive()?
            });
            if self.is_kw("PERCENT") {
                return Err(unsupported("FETCH ... PERCENT"));
            }
            if !self.eat_kw("ROWS") {
                self.expect_kw("ROW")?;
            }
            if !self.eat_kw("ONLY") {
                self.expect_kw("WITH")?;
                self.expect_kw("TIES")?;
                return Err(unsupported("FETCH ... WITH TIES"));
            }
        }
        if self.eat_kw("FOR") {
            // FOR UPDATE [OF ...] [NOWAIT | WAIT n | SKIP LOCKED]: rows are never locked.
            self.expect_kw("UPDATE")?;
            while matches!(
                self.peek(),
                Some(Tok::Ident(_))
                    | Some(Tok::QuotedIdent(_))
                    | Some(Tok::Number(_))
                    | Some(Tok::Symbol(","))
                    | Some(Tok::Symbol("."))
            ) {
                self.advance();
            }
        }
        Ok(Query {
            body,
            order_by,
            offset,
            fetch,
        })
    }

    fn set_expr(&mut self) -> Result<SetExpr, OraError> {
        let mut left = self.set_operand()?;
        loop {
            let op = if self.eat_kw("UNION") {
                if self.eat_kw("ALL") {
                    SetOp::UnionAll
                } else {
                    SetOp::Union
                }
            } else if self.eat_kw("INTERSECT") {
                SetOp::Intersect
            } else if self.eat_kw("MINUS") || self.eat_kw("EXCEPT") {
                SetOp::Minus
            } else {
                return Ok(left);
            };
            let right = self.set_operand()?;
            left = SetExpr::SetOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            };
        }
    }

    fn set_operand(&mut self) -> Result<SetExpr, OraError> {
        if !self.eat_sym("(") {
            return Ok(SetExpr::Select(Box::new(self.select()?)));
        }
        let q = self.query()?;
        self.expect_sym(")")?;
        if q.order_by.is_empty() && q.offset.is_none() && q.fetch.is_none() {
            return Ok(q.body);
        }
        // A parenthesised query with its own ORDER BY or paging: wrap it as an inline view.
        Ok(SetExpr::Select(Box::new(Select {
            distinct: false,
            items: vec![SelectItem::Star],
            from: vec![FromItem {
                factor: TableFactor::Subquery {
                    query: Box::new(q),
                    alias: None,
                },
                joins: Vec::new(),
            }],
            where_: None,
            group_by: Vec::new(),
            having: None,
        })))
    }

    fn select(&mut self) -> Result<Select, OraError> {
        if !self.eat_kw("SELECT") {
            return Err(missing_keyword());
        }
        let distinct = self.eat_kw("DISTINCT") || self.eat_kw("UNIQUE");
        if !distinct {
            self.eat_kw("ALL");
        }
        let mut items = vec![self.select_item()?];
        while self.eat_sym(",") {
            items.push(self.select_item()?);
        }
        if !self.eat_kw("FROM") {
            return Err(OraError::new(923, "FROM keyword not found where expected"));
        }
        let mut from = vec![self.source_item()?];
        while self.eat_sym(",") {
            from.push(self.source_item()?);
        }
        let where_ = if self.eat_kw("WHERE") {
            Some(self.expr()?)
        } else {
            None
        };
        if self.is_kw("CONNECT") || self.is_kw("START") {
            return Err(unsupported("CONNECT BY"));
        }
        let mut group_by = Vec::new();
        if self.eat_kw("GROUP") {
            self.expect_kw("BY")?;
            group_by.push(self.expr()?);
            while self.eat_sym(",") {
                group_by.push(self.expr()?);
            }
        }
        let having = if self.eat_kw("HAVING") {
            Some(self.expr()?)
        } else {
            None
        };
        Ok(Select {
            distinct,
            items,
            from,
            where_,
            group_by,
            having,
        })
    }

    fn select_item(&mut self) -> Result<SelectItem, OraError> {
        if self.eat_sym("*") {
            return Ok(SelectItem::Star);
        }
        if self.is_ident_at(0) && self.is_sym_at(1, ".") && self.is_sym_at(2, "*") {
            let table = self.ident()?;
            self.pos += 2;
            return Ok(SelectItem::QualifiedStar(table));
        }
        let start = self.tokens.get(self.pos).map(|t| t.start);
        let expr = self.expr()?;
        let end = self.tokens[self.pos - 1].end;
        let name = match (self.optional_alias()?, &expr) {
            (Some(alias), _) => alias,
            (None, Expr::Column { name, .. }) => name.clone(),
            // Oracle names the column after the expression text, uppercased, whitespace removed.
            (None, _) => {
                let text = &self.sql[start.unwrap_or(end)..end];
                text.chars()
                    .filter(|c| !c.is_whitespace())
                    .flat_map(char::to_uppercase)
                    .take(128)
                    .collect()
            }
        };
        Ok(SelectItem::Expr { expr, name })
    }

    fn source_item(&mut self) -> Result<FromItem, OraError> {
        let factor = self.table_factor()?;
        let mut joins = Vec::new();
        loop {
            let kind = if self.eat_kw("JOIN") {
                JoinKind::Inner
            } else if self.eat_kw("INNER") {
                self.expect_kw("JOIN")?;
                JoinKind::Inner
            } else if self.is_kw("LEFT") || self.is_kw("RIGHT") || self.is_kw("FULL") {
                let kind = match self.advance() {
                    Some(Tok::Ident(k)) if k == "LEFT" => JoinKind::Left,
                    Some(Tok::Ident(k)) if k == "RIGHT" => JoinKind::Right,
                    _ => JoinKind::Full,
                };
                self.eat_kw("OUTER");
                self.expect_kw("JOIN")?;
                kind
            } else if self.eat_kw("CROSS") {
                self.expect_kw("JOIN")?;
                JoinKind::Cross
            } else if self.is_kw("NATURAL") {
                return Err(unsupported("NATURAL JOIN"));
            } else {
                break;
            };
            let factor = self.table_factor()?;
            let on = if kind == JoinKind::Cross {
                None
            } else if self.eat_kw("ON") {
                Some(self.expr()?)
            } else if self.is_kw("USING") {
                return Err(unsupported("JOIN ... USING"));
            } else {
                return Err(missing_keyword());
            };
            joins.push(Join { kind, factor, on });
        }
        Ok(FromItem { factor, joins })
    }

    fn table_factor(&mut self) -> Result<TableFactor, OraError> {
        if self.eat_sym("(") {
            let query = self.query()?;
            self.expect_sym(")")?;
            let alias = self.optional_alias()?;
            return Ok(TableFactor::Subquery {
                query: Box::new(query),
                alias,
            });
        }
        let name = self.object_name()?;
        let alias = self.optional_alias()?;
        Ok(TableFactor::Table { name, alias })
    }

    // ---- expressions ----

    fn expr(&mut self) -> Result<Expr, OraError> {
        let mut left = self.and()?;
        while self.eat_kw("OR") {
            let right = self.and()?;
            left = Expr::Binary(BinaryOp::Or, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr, OraError> {
        let mut left = self.not()?;
        while self.eat_kw("AND") {
            let right = self.not()?;
            left = Expr::Binary(BinaryOp::And, Box::new(left), Box::new(right));
        }
        Ok(left)
    }

    fn not(&mut self) -> Result<Expr, OraError> {
        if self.eat_kw("NOT") {
            return Ok(Expr::Not(Box::new(self.not()?)));
        }
        self.predicate()
    }

    fn predicate(&mut self) -> Result<Expr, OraError> {
        let left = self.additive()?;
        let cmp = match self.peek() {
            Some(Tok::Symbol("=")) => Some(BinaryOp::Eq),
            Some(Tok::Symbol("<>")) | Some(Tok::Symbol("!=")) | Some(Tok::Symbol("^=")) => {
                Some(BinaryOp::NotEq)
            }
            Some(Tok::Symbol("<")) => Some(BinaryOp::Lt),
            Some(Tok::Symbol("<=")) => Some(BinaryOp::LtEq),
            Some(Tok::Symbol(">")) => Some(BinaryOp::Gt),
            Some(Tok::Symbol(">=")) => Some(BinaryOp::GtEq),
            _ => None,
        };
        if let Some(op) = cmp {
            self.advance();
            if self.is_kw("ANY") || self.is_kw("SOME") || self.is_kw("ALL") {
                return Err(unsupported("ANY/ALL comparisons"));
            }
            let right = self.additive()?;
            if self.is_sym("(") && self.is_sym_at(1, "+") {
                return Err(unsupported("(+) outer joins"));
            }
            return Ok(Expr::Binary(op, Box::new(left), Box::new(right)));
        }
        if self.eat_kw("IS") {
            let negated = self.eat_kw("NOT");
            self.expect_kw("NULL")?;
            return Ok(Expr::IsNull {
                expr: Box::new(left),
                negated,
            });
        }
        let negated = self.is_kw("NOT")
            && ["BETWEEN", "IN", "LIKE"]
                .iter()
                .any(|k| self.is_kw_at(1, k));
        if negated {
            self.advance();
        }
        if self.eat_kw("BETWEEN") {
            let low = self.additive()?;
            self.expect_kw("AND")?;
            let high = self.additive()?;
            return Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            });
        }
        if self.eat_kw("IN") {
            self.expect_sym("(")?;
            if self.is_kw("SELECT") {
                let query = self.query()?;
                self.expect_sym(")")?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(left),
                    query: Box::new(query),
                    negated,
                });
            }
            let mut list = vec![self.expr()?];
            while self.eat_sym(",") {
                list.push(self.expr()?);
            }
            self.expect_sym(")")?;
            return Ok(Expr::InList {
                expr: Box::new(left),
                list,
                negated,
            });
        }
        if self.eat_kw("LIKE") {
            let pattern = self.additive()?;
            let escape = if self.eat_kw("ESCAPE") {
                Some(Box::new(self.additive()?))
            } else {
                None
            };
            return Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                escape,
                negated,
            });
        }
        Ok(left)
    }

    fn additive(&mut self) -> Result<Expr, OraError> {
        let mut left = self.term()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Symbol("+")) => BinaryOp::Add,
                Some(Tok::Symbol("-")) => BinaryOp::Sub,
                Some(Tok::Symbol("||")) => BinaryOp::Concat,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.term()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn term(&mut self) -> Result<Expr, OraError> {
        let mut left = self.unary()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Symbol("*")) => BinaryOp::Mul,
                Some(Tok::Symbol("/")) => BinaryOp::Div,
                _ => return Ok(left),
            };
            self.advance();
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, OraError> {
        if self.eat_sym("-") {
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.eat_sym("+") {
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, OraError> {
        match self.peek().cloned() {
            Some(Tok::Number(n)) => {
                self.advance();
                Value::parse_number(&n)
                    .map(Expr::Literal)
                    .ok_or_else(|| OraError::new(1722, "invalid number"))
            }
            Some(Tok::Str(s)) => {
                self.advance();
                Ok(Expr::Literal(Value::varchar(s)))
            }
            Some(Tok::Bind(_)) => {
                self.advance();
                self.binds += 1;
                Ok(Expr::Bind(self.binds - 1))
            }
            Some(Tok::Symbol("(")) => {
                self.advance();
                if self.is_kw("SELECT") {
                    let q = self.query()?;
                    self.expect_sym(")")?;
                    return Ok(Expr::Subquery(Box::new(q)));
                }
                let e = self.expr()?;
                self.expect_sym(")")?;
                Ok(e)
            }
            Some(Tok::Ident(k)) => self.keyword_or_identifier(k),
            Some(Tok::QuotedIdent(_)) => self.column_ref(),
            _ => Err(OraError::missing_expression()),
        }
    }

    fn string_after_keyword(&mut self) -> String {
        self.advance();
        match self.advance() {
            Some(Tok::Str(s)) => s,
            _ => unreachable!("checked by the caller"),
        }
    }

    fn keyword_or_identifier(&mut self, k: String) -> Result<Expr, OraError> {
        let next_is_str = matches!(self.peek_at(1), Some(Tok::Str(_)));
        match k.as_str() {
            "NULL" => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            "DATE" if next_is_str => {
                let s = self.string_after_keyword();
                Ok(Expr::Literal(Value::Date(datetime::parse(
                    &s,
                    "YYYY-MM-DD",
                )?)))
            }
            "TIMESTAMP" if next_is_str => {
                let s = self.string_after_keyword();
                let fmt = if s.contains('.') {
                    "YYYY-MM-DD HH24:MI:SS.FF"
                } else {
                    "YYYY-MM-DD HH24:MI:SS"
                };
                Ok(Expr::Literal(Value::Timestamp(datetime::parse(&s, fmt)?)))
            }
            "INTERVAL" if next_is_str => Err(unsupported("INTERVAL literals")),
            "CASE" => {
                self.advance();
                self.case()
            }
            "EXISTS" if self.is_sym_at(1, "(") => {
                self.pos += 2;
                let q = self.query()?;
                self.expect_sym(")")?;
                Ok(Expr::Exists(Box::new(q)))
            }
            "ROWNUM" => {
                self.advance();
                Ok(Expr::RowNum)
            }
            "SYSDATE" | "SYSTIMESTAMP" | "CURRENT_DATE" | "CURRENT_TIMESTAMP"
            | "LOCALTIMESTAMP" | "USER"
                if !self.is_sym_at(1, "(") && !self.is_sym_at(1, ".") =>
            {
                self.advance();
                Ok(Expr::Function {
                    name: k,
                    args: Vec::new(),
                    distinct: false,
                })
            }
            _ if RESERVED.contains(&k.as_str()) && !self.is_sym_at(1, "(") => {
                Err(OraError::missing_expression())
            }
            _ if self.is_sym_at(1, "(") => self.function(k),
            _ => self.column_ref(),
        }
    }

    fn column_ref(&mut self) -> Result<Expr, OraError> {
        let first = self.ident()?;
        if !(self.is_sym(".") && self.is_ident_at(1)) {
            return Ok(Expr::Column {
                table: None,
                name: first,
            });
        }
        self.advance();
        let second = self.ident()?;
        if matches!(second.as_str(), "NEXTVAL" | "CURRVAL") {
            return Err(unsupported("sequences"));
        }
        // schema.table.column: keep the table qualifier.
        if self.is_sym(".") && self.is_ident_at(1) {
            self.advance();
            let third = self.ident()?;
            return Ok(Expr::Column {
                table: Some(second),
                name: third,
            });
        }
        Ok(Expr::Column {
            table: Some(first),
            name: second,
        })
    }

    fn case(&mut self) -> Result<Expr, OraError> {
        let operand = if self.is_kw("WHEN") {
            None
        } else {
            Some(Box::new(self.expr()?))
        };
        let mut whens = Vec::new();
        while self.eat_kw("WHEN") {
            let cond = self.expr()?;
            self.expect_kw("THEN")?;
            whens.push((cond, self.expr()?));
        }
        if whens.is_empty() {
            return Err(missing_keyword());
        }
        let else_ = if self.eat_kw("ELSE") {
            Some(Box::new(self.expr()?))
        } else {
            None
        };
        self.expect_kw("END")?;
        Ok(Expr::Case {
            operand,
            whens,
            else_,
        })
    }

    fn function(&mut self, name: String) -> Result<Expr, OraError> {
        self.pos += 2; // name and "("
        let lit = |s: &str| Expr::Literal(Value::varchar(s));
        let call = |name: &str, args: Vec<Expr>| Expr::Function {
            name: name.into(),
            args,
            distinct: false,
        };
        match name.as_str() {
            "COUNT" if self.is_sym("*") => {
                self.advance();
                self.expect_sym(")")?;
                return self.no_over(Expr::CountStar);
            }
            "CAST" => {
                let e = self.expr()?;
                self.expect_kw("AS")?;
                let target = match self.data_type()? {
                    SqlType::Number { .. } => "NUMBER".to_string(),
                    SqlType::Varchar2(n) | SqlType::Char(n) => format!("VARCHAR2:{n}"),
                    SqlType::Date => "DATE".to_string(),
                    SqlType::Timestamp(_) => "TIMESTAMP".to_string(),
                };
                self.expect_sym(")")?;
                return Ok(call("CAST", vec![e, lit(&target)]));
            }
            "EXTRACT" => {
                let field = self.ident()?;
                self.expect_kw("FROM")?;
                let e = self.expr()?;
                self.expect_sym(")")?;
                return Ok(call("EXTRACT", vec![lit(&field), e]));
            }
            "TRIM" => {
                // Normalised to TRIM(mode, chars, string).
                let mode = ["LEADING", "TRAILING", "BOTH"]
                    .into_iter()
                    .find(|m| self.eat_kw(m));
                let mut chars = lit(" ");
                let s = if mode.is_some() && self.eat_kw("FROM") {
                    self.expr()?
                } else {
                    let first = self.expr()?;
                    if self.eat_kw("FROM") {
                        chars = first;
                        self.expr()?
                    } else {
                        first
                    }
                };
                self.expect_sym(")")?;
                return Ok(call("TRIM", vec![lit(mode.unwrap_or("BOTH")), chars, s]));
            }
            _ => {}
        }
        let distinct = self.eat_kw("DISTINCT");
        if !distinct {
            self.eat_kw("ALL");
        }
        let mut args = Vec::new();
        if !self.is_sym(")") {
            args.push(self.expr()?);
            while self.eat_sym(",") {
                args.push(self.expr()?);
            }
        }
        self.expect_sym(")")?;
        self.no_over(Expr::Function {
            name,
            args,
            distinct,
        })
    }

    fn no_over(&self, e: Expr) -> Result<Expr, OraError> {
        if self.is_kw("OVER") || self.is_kw("WITHIN") || self.is_kw("KEEP") {
            return Err(unsupported("analytic functions"));
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<String> {
        match parse(sql).unwrap().stmt {
            Statement::Query(Query {
                body: SetExpr::Select(s),
                ..
            }) => s
                .items
                .into_iter()
                .map(|i| match i {
                    SelectItem::Expr { name, .. } => name,
                    SelectItem::Star => "*".into(),
                    SelectItem::QualifiedStar(t) => format!("{t}.*"),
                })
                .collect(),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn column_names_follow_oracle() {
        assert_eq!(names("select 1 from dual"), ["1"]);
        assert_eq!(names("select 1 + 1, 'ab' from dual"), ["1+1", "'AB'"]);
        assert_eq!(
            names("select 1 as one, 2 two, 3 \"Three\" from dual"),
            ["ONE", "TWO", "Three"]
        );
        assert_eq!(
            names("select e.name, x.* from emp e, dual x"),
            ["NAME", "X.*"]
        );
    }

    #[test]
    fn binds_are_numbered_by_occurrence() {
        assert_eq!(parse("select :a, :b, :a from dual").unwrap().binds, 3);
        assert_eq!(parse("update t set a = :1 where b = :2").unwrap().binds, 2);
    }

    #[test]
    fn parses_statements() {
        for sql in [
            "select distinct a, count(*) from t where a > 1 and b like 'x%' group by a having count(*) > 1 order by 2 desc nulls last",
            "select * from a join b on a.id = b.id left outer join c on c.id = b.id where exists (select 1 from d where d.id = a.id)",
            "select * from t order by id offset 10 rows fetch next 5 rows only",
            "select a from t union all select b from u minus select c from v",
            "select case when x is null then 'n' else 'y' end, nvl(x, 0), trim(both ' ' from y), trim(y), cast(z as varchar2(10)) from t",
            "select * from (select * from t where rownum <= 3) q",
            "select * from t for update nowait",
            "select count(distinct a), extract(year from d), date '2024-01-31', timestamp '2024-01-31 10:00:00.5' from t",
            "insert into t (a, b) values (1, 'x')",
            "insert into t select * from u",
            "update t x set x.a = x.a + 1, b = default_b where id in (1, 2)",
            "delete from t where not (a between 1 and 3)",
            "delete t where a not in (select a from u)",
            "create table t (id number(10) constraint t_pk primary key, name varchar2(50 char) not null, created date default sysdate, amount number(12,2) check (amount >= 0), code char(3) unique, ts timestamp(3) with time zone) tablespace users",
            "create table t (id integer generated by default on null as identity (start with 1), parent_id number references p(id) on delete cascade, constraint t_uk unique (parent_id))",
            "create table t as select * from u",
            "create unique index t_ix on t (a, b desc)",
            "drop table t cascade constraints purge",
            "truncate table t",
            "alter session set nls_date_format = 'YYYY-MM-DD'",
            "commit work",
        ] {
            if let Err(e) = parse(sql) {
                panic!("{sql}: {e}");
            }
        }
    }

    #[test]
    fn errors_match_oracle() {
        let code = |sql: &str| parse(sql).err().map(|e| e.code);
        assert_eq!(code("select 1"), Some(923));
        assert_eq!(code("select from dual"), Some(936));
        assert_eq!(code("select 1 from dual;"), Some(911));
        assert_eq!(code("select (1 from dual"), Some(907));
        assert_eq!(code("select 1 from dual x y"), Some(933));
        assert_eq!(code("frob"), Some(900));
        assert_eq!(code("create table t (a varchar2)"), Some(906));
        assert_eq!(code("create table t (a blob)"), Some(902));
        assert_eq!(code("insert into t (a) select"), Some(936));
    }
}
