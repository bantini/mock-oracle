//! Recursive-descent parser for the supported subset of Oracle SQL.

use crate::ast::{BinaryOp, Expr, Select, SelectItem, Statement, TableRef};
use crate::lexer::{tokenize, Tok, Token};
use crate::{OraError, Value};

pub fn parse(sql: &str) -> Result<Statement, OraError> {
    let tokens = tokenize(sql)?;
    let mut p = Parser {
        sql,
        tokens,
        pos: 0,
        binds: 0,
    };
    let stmt = match p.peek() {
        Some(Tok::Ident(k)) if k == "SELECT" => Statement::Select(p.select()?),
        _ => return Err(OraError::new(900, "invalid SQL statement")),
    };
    match p.peek() {
        None => Ok(stmt),
        Some(Tok::Symbol(";")) => Err(OraError::new(911, "invalid character")),
        Some(_) => Err(OraError::new(933, "SQL command not properly ended")),
    }
}

/// Counts how many bind placeholders a statement has, in order of appearance.
pub fn count_binds(stmt: &Statement) -> usize {
    fn walk(e: &Expr, max: &mut usize) {
        match e {
            Expr::Bind(i) => *max = (*max).max(i + 1),
            Expr::Neg(inner) => walk(inner, max),
            Expr::Binary(_, l, r) => {
                walk(l, max);
                walk(r, max);
            }
            Expr::Literal(_) | Expr::Column(_) | Expr::Star => {}
        }
    }
    let mut max = 0;
    match stmt {
        Statement::Select(s) => s.items.iter().for_each(|i| walk(&i.expr, &mut max)),
    }
    max
}

struct Parser<'a> {
    sql: &'a str,
    tokens: Vec<Token>,
    pos: usize,
    binds: usize,
}

impl Parser<'_> {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos).map(|t| &t.tok)
    }

    fn is_keyword(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(k)) if k == kw)
    }

    fn is_symbol(&self, sym: &str) -> bool {
        matches!(self.peek(), Some(Tok::Symbol(s)) if *s == sym)
    }

    fn advance(&mut self) -> Option<Token> {
        let t = self.tokens.get(self.pos).cloned();
        self.pos += 1;
        t
    }

    fn select(&mut self) -> Result<Select, OraError> {
        self.advance(); // SELECT
        let mut items = vec![self.select_item()?];
        while self.is_symbol(",") {
            self.advance();
            items.push(self.select_item()?);
        }
        if !self.is_keyword("FROM") {
            return Err(OraError::new(923, "FROM keyword not found where expected"));
        }
        self.advance();
        let from = match self.advance().map(|t| t.tok) {
            Some(Tok::Ident(name)) | Some(Tok::QuotedIdent(name)) if name == "DUAL" => {
                TableRef::Dual
            }
            Some(Tok::Ident(_)) | Some(Tok::QuotedIdent(_)) => {
                return Err(OraError::new(942, "table or view does not exist"))
            }
            _ => return Err(OraError::new(903, "invalid table name")),
        };
        Ok(Select { items, from })
    }

    fn select_item(&mut self) -> Result<SelectItem, OraError> {
        if self.is_symbol("*") {
            self.advance();
            return Ok(SelectItem {
                expr: Expr::Star,
                name: "*".into(),
            });
        }
        let start = self.tokens.get(self.pos).map(|t| t.start);
        let expr = self.expr()?;
        let end = self.tokens[self.pos - 1].end;
        let text = &self.sql[start.unwrap_or(end)..end];
        let mut name: String = text
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect::<String>()
            .to_uppercase();

        if self.is_keyword("AS") {
            self.advance();
            name = self.alias()?;
        } else if matches!(self.peek(), Some(Tok::QuotedIdent(_)))
            || matches!(self.peek(), Some(Tok::Ident(k)) if k != "FROM")
        {
            name = self.alias()?;
        }
        Ok(SelectItem { expr, name })
    }

    fn alias(&mut self) -> Result<String, OraError> {
        match self.advance().map(|t| t.tok) {
            Some(Tok::Ident(n)) | Some(Tok::QuotedIdent(n)) => Ok(n),
            _ => Err(OraError::new(923, "FROM keyword not found where expected")),
        }
    }

    // expr := term (('+' | '-' | '||') term)*
    fn expr(&mut self) -> Result<Expr, OraError> {
        let mut left = self.term()?;
        loop {
            let op = if self.is_symbol("+") {
                BinaryOp::Add
            } else if self.is_symbol("-") {
                BinaryOp::Sub
            } else if self.is_symbol("||") {
                BinaryOp::Concat
            } else {
                return Ok(left);
            };
            self.advance();
            let right = self.term()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    // term := unary (('*' | '/') unary)*
    fn term(&mut self) -> Result<Expr, OraError> {
        let mut left = self.unary()?;
        loop {
            let op = if self.is_symbol("*") {
                BinaryOp::Mul
            } else if self.is_symbol("/") {
                BinaryOp::Div
            } else {
                return Ok(left);
            };
            self.advance();
            let right = self.unary()?;
            left = Expr::Binary(op, Box::new(left), Box::new(right));
        }
    }

    fn unary(&mut self) -> Result<Expr, OraError> {
        if self.is_symbol("-") {
            self.advance();
            return Ok(Expr::Neg(Box::new(self.unary()?)));
        }
        if self.is_symbol("+") {
            self.advance();
            return self.unary();
        }
        self.primary()
    }

    fn primary(&mut self) -> Result<Expr, OraError> {
        let missing = || OraError::new(936, "missing expression");
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
            Some(Tok::Ident(k)) if k == "NULL" => {
                self.advance();
                Ok(Expr::Literal(Value::Null))
            }
            Some(Tok::Ident(k)) if k == "FROM" => Err(missing()),
            Some(Tok::Ident(name)) | Some(Tok::QuotedIdent(name)) => {
                self.advance();
                Ok(Expr::Column(name))
            }
            Some(Tok::Symbol("(")) => {
                self.advance();
                let e = self.expr()?;
                if !self.is_symbol(")") {
                    return Err(OraError::new(907, "missing right parenthesis"));
                }
                self.advance();
                Ok(e)
            }
            _ => Err(missing()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(sql: &str) -> Vec<String> {
        match parse(sql).unwrap() {
            Statement::Select(s) => s.items.into_iter().map(|i| i.name).collect(),
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
        assert_eq!(names("select * from dual"), ["*"]);
    }

    #[test]
    fn binds_are_numbered_by_occurrence() {
        let stmt = parse("select :a, :b, :a from dual").unwrap();
        assert_eq!(count_binds(&stmt), 3);
    }

    #[test]
    fn errors_match_oracle() {
        assert_eq!(parse("select 1").unwrap_err().code, 923);
        assert_eq!(parse("select from dual").unwrap_err().code, 936);
        assert_eq!(parse("select 1 from users").unwrap_err().code, 942);
        assert_eq!(parse("select 1 from dual;").unwrap_err().code, 911);
        assert_eq!(parse("select (1 from dual").unwrap_err().code, 907);
        assert_eq!(parse("frob").unwrap_err().code, 900);
    }
}
