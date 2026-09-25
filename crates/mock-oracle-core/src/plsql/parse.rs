//! Parsing PL/SQL blocks, procedures and functions. SQL statements and expressions
//! inside them are parsed by the SQL parser this extends.

use std::sync::Arc;

use super::*;
use crate::plsql::*;

/// Words that end a list of statements.
const STATEMENT_LIST_END: &[&str] = &["END", "EXCEPTION", "ELSIF", "ELSE", "WHEN"];

/// ORA-06550 wrapping a PLS- compilation error, as Oracle reports them.
pub(crate) fn pls(code: u32, message: &str) -> OraError {
    OraError::new(6550, format!("PLS-{code:05}: {message}"))
}

fn expected(what: &str) -> OraError {
    pls(
        103,
        &format!("Encountered an unexpected symbol when expecting {what}"),
    )
}

impl Parser<'_> {
    /// `[<<label>>] [DECLARE declarations] BEGIN statements [EXCEPTION handlers] END [label]`
    pub(super) fn block(&mut self) -> Result<Block, OraError> {
        self.label()?;
        let decls = if self.eat_kw("DECLARE") {
            self.declarations()?
        } else {
            Vec::new()
        };
        self.block_body(decls)
    }

    /// `BEGIN ... END [name]`, after the declarations.
    fn block_body(&mut self, decls: Vec<Decl>) -> Result<Block, OraError> {
        if !self.eat_kw("BEGIN") {
            return Err(expected("BEGIN"));
        }
        let body = self.statements()?;
        let mut handlers = Vec::new();
        if self.eat_kw("EXCEPTION") {
            while self.eat_kw("WHEN") {
                let mut exceptions = vec![self.dotted_name()?];
                while self.eat_kw("OR") {
                    exceptions.push(self.dotted_name()?);
                }
                self.expect_kw("THEN")?;
                handlers.push(Handler {
                    exceptions,
                    body: self.statements()?,
                });
            }
        }
        if !self.eat_kw("END") {
            return Err(expected("END"));
        }
        if self.is_ident_at(0) {
            self.advance();
        }
        Ok(Block {
            decls,
            body,
            handlers,
        })
    }

    /// `CALL name(args) [INTO :bind]`, run as a block.
    pub(super) fn call_statement(&mut self) -> Result<Block, OraError> {
        self.advance();
        let name = self.dotted_name()?;
        let args = self.args()?;
        let body = if self.eat_kw("INTO") {
            let target = match self.primary()? {
                Expr::Bind(i) => Target::Bind(i),
                _ => return Err(expected("a bind variable")),
            };
            Stmt::Assign {
                target,
                value: Expr::Function {
                    name,
                    args: args.into_iter().map(|a| a.value).collect(),
                    distinct: false,
                },
            }
        } else {
            Stmt::Call { name, args }
        };
        Ok(Block {
            decls: Vec::new(),
            body: vec![body],
            handlers: Vec::new(),
        })
    }

    fn label(&mut self) -> Result<Option<String>, OraError> {
        if !self.eat_sym("<<") {
            return Ok(None);
        }
        let name = self.ident()?;
        if !self.eat_sym(">>") {
            return Err(expected(">>"));
        }
        Ok(Some(name))
    }

    /// `a.b.c` as one name.
    fn dotted_name(&mut self) -> Result<String, OraError> {
        let mut name = self.ident()?;
        while self.is_sym(".") && self.is_ident_at(1) {
            self.advance();
            name.push('.');
            name.push_str(&self.ident()?);
        }
        Ok(name)
    }

    fn end_semicolon(&mut self) -> Result<(), OraError> {
        if self.eat_sym(";") {
            Ok(())
        } else {
            Err(expected(";"))
        }
    }

    // ---- declarations ----

    fn declarations(&mut self) -> Result<Vec<Decl>, OraError> {
        let mut decls = Vec::new();
        while !self.is_kw("BEGIN") {
            if self.peek().is_none() {
                return Err(expected("BEGIN"));
            }
            if self.is_kw("PROCEDURE") || self.is_kw("FUNCTION") {
                let routine = self.routine_signature()?;
                if self.eat_sym(";") {
                    continue; // forward declaration
                }
                let routine = self.routine_body(routine)?;
                self.end_semicolon()?;
                decls.push(Decl::Routine(Arc::new(routine)));
                continue;
            }
            if self.eat_kw("CURSOR") {
                let name = self.ident()?;
                let params = self.params()?;
                if self.is_kw("RETURN") {
                    return Err(unsupported("typed cursors"));
                }
                self.expect_kw("IS")?;
                let query = self.query()?;
                self.end_semicolon()?;
                decls.push(Decl::Cursor(CursorDecl {
                    name,
                    params,
                    query,
                }));
                continue;
            }
            if self.eat_kw("PRAGMA") {
                let pragma = self.ident()?;
                if pragma == "EXCEPTION_INIT" {
                    self.expect_sym("(")?;
                    let name = self.ident()?;
                    self.expect_sym(",")?;
                    let code = self.integer()?;
                    self.expect_sym(")")?;
                    decls.push(Decl::ExceptionInit {
                        name,
                        code: code.unsigned_abs() as u32,
                    });
                } else {
                    // AUTONOMOUS_TRANSACTION, SERIALLY_REUSABLE and the like are ignored.
                    self.skip_parens()?;
                }
                self.end_semicolon()?;
                continue;
            }
            if self.is_kw("TYPE") || self.is_kw("SUBTYPE") {
                return Err(unsupported("user-defined PL/SQL types"));
            }
            let name = self.ident()?;
            if self.eat_kw("EXCEPTION") {
                self.end_semicolon()?;
                decls.push(Decl::Exception(name));
                continue;
            }
            let constant = self.eat_kw("CONSTANT");
            let ty = self.type_ref(false)?;
            let not_null = if self.eat_kw("NOT") {
                self.expect_kw("NULL")?;
                true
            } else {
                false
            };
            let default = if self.eat_sym(":=") || self.eat_kw("DEFAULT") {
                Some(self.expr()?)
            } else {
                None
            };
            if (constant || not_null) && default.is_none() {
                return Err(pls(
                    218,
                    "a variable declared NOT NULL must have an initialization assignment",
                ));
            }
            self.end_semicolon()?;
            decls.push(Decl::Var(VarDecl {
                name,
                ty,
                constant,
                not_null,
                default,
            }));
        }
        Ok(decls)
    }

    /// A PL/SQL type. Parameters may leave out lengths (`VARCHAR2` alone).
    fn type_ref(&mut self, param: bool) -> Result<TypeRef, OraError> {
        if self.is_ident_at(0) && (self.is_sym_at(1, "%") || self.is_sym_at(1, ".")) {
            let first = self.ident()?;
            let second = if self.eat_sym(".") {
                Some(self.ident()?)
            } else {
                None
            };
            if !self.eat_sym("%") {
                return Err(expected("%TYPE or %ROWTYPE"));
            }
            let attr = self.ident()?;
            return match (attr.as_str(), second) {
                ("TYPE", Some(column)) => Ok(TypeRef::TypeOf {
                    table: Some(first),
                    name: column,
                }),
                ("TYPE", None) => Ok(TypeRef::TypeOf {
                    table: None,
                    name: first,
                }),
                ("ROWTYPE", None) => Ok(TypeRef::RowType(first)),
                ("ROWTYPE", Some(table)) => Ok(TypeRef::RowType(table)),
                _ => Err(expected("%TYPE or %ROWTYPE")),
            };
        }
        let Some(Tok::Ident(name)) = self.peek().cloned() else {
            return Err(OraError::new(902, "invalid datatype"));
        };
        match name.as_str() {
            "BOOLEAN" => {
                self.advance();
                Ok(TypeRef::Boolean)
            }
            "PLS_INTEGER" | "BINARY_INTEGER" | "NATURAL" | "NATURALN" | "POSITIVE"
            | "POSITIVEN" | "SIMPLE_INTEGER" | "SIGNTYPE" => {
                self.advance();
                Ok(TypeRef::Integer)
            }
            "STRING" => {
                self.advance();
                let n = self.length()?.unwrap_or(32767);
                Ok(TypeRef::Sql(SqlType::Varchar2(n)))
            }
            "VARCHAR2" | "VARCHAR" | "NVARCHAR2" | "CHAR" | "NCHAR" if !self.is_sym_at(1, "(") => {
                let fixed = name == "CHAR" || name == "NCHAR";
                if !param && !fixed {
                    return Err(pls(
                        215,
                        "String length constraints must be in range (1 .. 32767)",
                    ));
                }
                self.advance();
                Ok(TypeRef::Sql(if fixed && !param {
                    SqlType::Char(1)
                } else {
                    SqlType::Varchar2(32767)
                }))
            }
            "SYS_REFCURSOR" | "REF" => Err(unsupported("REF CURSOR")),
            _ => Ok(TypeRef::Sql(self.data_type()?)),
        }
    }

    // ---- procedures and functions ----

    /// A full `PROCEDURE` or `FUNCTION` definition, as in CREATE PROCEDURE.
    pub(super) fn routine(&mut self) -> Result<Routine, OraError> {
        let r = self.routine_signature()?;
        self.routine_body(r)
    }

    fn routine_signature(&mut self) -> Result<Routine, OraError> {
        let function = self.eat_kw("FUNCTION");
        if !function {
            self.expect_kw("PROCEDURE")?;
        }
        let name = self.object_name()?;
        let params = self.params()?;
        let returns = if function {
            self.expect_kw("RETURN")?;
            Some(self.type_ref(true)?)
        } else {
            None
        };
        Ok(Routine {
            name,
            params,
            returns,
            block: Block {
                decls: Vec::new(),
                body: Vec::new(),
                handlers: Vec::new(),
            },
        })
    }

    fn routine_body(&mut self, mut r: Routine) -> Result<Routine, OraError> {
        // Options before IS / AS.
        loop {
            if self.eat_kw("AUTHID") {
                self.advance();
            } else if self.eat_kw("DETERMINISTIC")
                || self.eat_kw("PARALLEL_ENABLE")
                || self.eat_kw("RESULT_CACHE")
            {
                self.skip_parens()?;
            } else if self.is_kw("PIPELINED") {
                return Err(unsupported("pipelined functions"));
            } else {
                break;
            }
        }
        if !self.eat_kw("IS") && !self.eat_kw("AS") {
            return Err(expected("IS or AS"));
        }
        if self.is_kw("LANGUAGE") || self.is_kw("EXTERNAL") {
            return Err(unsupported("external procedures"));
        }
        let decls = self.declarations()?;
        r.block = self.block_body(decls)?;
        Ok(r)
    }

    fn params(&mut self) -> Result<Vec<Param>, OraError> {
        let mut params = Vec::new();
        if !self.eat_sym("(") {
            return Ok(params);
        }
        loop {
            let name = self.ident()?;
            let mode = if self.eat_kw("OUT") {
                ParamMode::Out
            } else if self.eat_kw("IN") {
                if self.eat_kw("OUT") {
                    ParamMode::InOut
                } else {
                    ParamMode::In
                }
            } else {
                ParamMode::In
            };
            self.eat_kw("NOCOPY");
            let ty = self.type_ref(true)?;
            let default = if self.eat_sym(":=") || self.eat_kw("DEFAULT") {
                Some(self.expr()?)
            } else {
                None
            };
            params.push(Param {
                name,
                mode,
                ty,
                default,
            });
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(params)
    }

    /// `(arg, name => arg, ...)`, or nothing.
    fn args(&mut self) -> Result<Vec<Arg>, OraError> {
        let mut args = Vec::new();
        if !self.eat_sym("(") {
            return Ok(args);
        }
        if self.eat_sym(")") {
            return Ok(args);
        }
        loop {
            let name = if self.is_ident_at(0) && self.is_sym_at(1, "=>") {
                let n = self.ident()?;
                self.advance();
                Some(n)
            } else {
                None
            };
            args.push(Arg {
                name,
                value: self.expr()?,
            });
            if !self.eat_sym(",") {
                break;
            }
        }
        self.expect_sym(")")?;
        Ok(args)
    }

    // ---- statements ----

    /// Statements up to END, EXCEPTION, ELSIF, ELSE or WHEN.
    fn statements(&mut self) -> Result<Vec<Stmt>, OraError> {
        let mut out = Vec::new();
        loop {
            match self.peek() {
                None => return Err(expected("END")),
                Some(Tok::Ident(k)) if STATEMENT_LIST_END.contains(&k.as_str()) => break,
                _ => out.push(self.plsql_statement()?),
            }
        }
        if out.is_empty() {
            return Err(expected("a statement"));
        }
        Ok(out)
    }

    fn plsql_statement(&mut self) -> Result<Stmt, OraError> {
        let label = self.label()?;
        let Some(tok) = self.peek().cloned() else {
            return Err(expected("a statement"));
        };
        let keyword = match &tok {
            Tok::Ident(k) => k.as_str(),
            Tok::Bind(_) => "",
            Tok::Symbol("(") => "SELECT",
            _ => return Err(expected("a statement")),
        };
        let stmt = match keyword {
            "NULL" => {
                self.advance();
                Stmt::Null
            }
            "IF" => {
                self.advance();
                let mut branches = Vec::new();
                let cond = self.expr()?;
                self.expect_kw("THEN")?;
                branches.push((cond, self.statements()?));
                let mut else_ = None;
                loop {
                    if self.eat_kw("ELSIF") {
                        let cond = self.expr()?;
                        self.expect_kw("THEN")?;
                        branches.push((cond, self.statements()?));
                    } else if self.eat_kw("ELSE") {
                        else_ = Some(self.statements()?);
                    } else {
                        break;
                    }
                }
                self.expect_kw("END")?;
                self.expect_kw("IF")?;
                Stmt::If { branches, else_ }
            }
            "CASE" => {
                self.advance();
                let operand = if self.is_kw("WHEN") {
                    None
                } else {
                    Some(self.expr()?)
                };
                let mut whens = Vec::new();
                while self.eat_kw("WHEN") {
                    let w = self.expr()?;
                    self.expect_kw("THEN")?;
                    whens.push((w, self.statements()?));
                }
                let else_ = if self.eat_kw("ELSE") {
                    Some(self.statements()?)
                } else {
                    None
                };
                self.expect_kw("END")?;
                self.expect_kw("CASE")?;
                if self.is_ident_at(0) {
                    self.advance();
                }
                Stmt::Case {
                    operand,
                    whens,
                    else_,
                }
            }
            "LOOP" => {
                let body = self.loop_body()?;
                Stmt::Loop { label, body }
            }
            "WHILE" => {
                self.advance();
                let cond = self.expr()?;
                let body = self.loop_body()?;
                Stmt::While { label, cond, body }
            }
            "FOR" => {
                self.advance();
                let var = self.ident()?;
                self.expect_kw("IN")?;
                let reverse = self.eat_kw("REVERSE");
                if !reverse
                    && self.is_sym("(")
                    && (self.is_kw_at(1, "SELECT") || self.is_kw_at(1, "WITH"))
                {
                    self.advance();
                    let q = self.query()?;
                    self.expect_sym(")")?;
                    let body = self.loop_body()?;
                    Stmt::ForCursor {
                        label,
                        var,
                        source: CursorSource::Query(q),
                        body,
                    }
                } else {
                    let low = self.additive()?;
                    if self.eat_sym("..") {
                        let high = self.additive()?;
                        let body = self.loop_body()?;
                        Stmt::ForRange {
                            label,
                            var,
                            reverse,
                            low,
                            high,
                            body,
                        }
                    } else {
                        let source = match low {
                            Expr::Column { table: None, name } => CursorSource::Named {
                                name,
                                args: Vec::new(),
                            },
                            Expr::Function { name, args, .. } => CursorSource::Named { name, args },
                            _ => return Err(expected("..")),
                        };
                        let body = self.loop_body()?;
                        Stmt::ForCursor {
                            label,
                            var,
                            source,
                            body,
                        }
                    }
                }
            }
            "EXIT" | "CONTINUE" => {
                self.advance();
                let target = if self.is_ident_at(0) && !self.is_kw("WHEN") {
                    Some(self.ident()?)
                } else {
                    None
                };
                let when = if self.eat_kw("WHEN") {
                    Some(self.expr()?)
                } else {
                    None
                };
                if keyword == "EXIT" {
                    Stmt::Exit {
                        label: target,
                        when,
                    }
                } else {
                    Stmt::Continue {
                        label: target,
                        when,
                    }
                }
            }
            "RETURN" => {
                self.advance();
                if self.is_sym(";") {
                    Stmt::Return(None)
                } else {
                    Stmt::Return(Some(self.expr()?))
                }
            }
            "RAISE" => {
                self.advance();
                if self.is_sym(";") {
                    Stmt::Raise(None)
                } else {
                    Stmt::Raise(Some(self.dotted_name()?))
                }
            }
            "BEGIN" | "DECLARE" => Stmt::Block(self.block()?),
            "SELECT" | "WITH" => {
                self.into_allowed = true;
                self.into = None;
                let q = self.query();
                self.into_allowed = false;
                let q = q?;
                let Some(into) = self.into.take() else {
                    return Err(pls(
                        428,
                        "an INTO clause is expected in this SELECT statement",
                    ));
                };
                Stmt::Sql {
                    stmt: Statement::Query(q),
                    into: into.into_iter().map(target).collect::<Result<_, _>>()?,
                }
            }
            "INSERT" | "UPDATE" | "DELETE" | "COMMIT" | "ROLLBACK" => Stmt::Sql {
                stmt: self.statement()?,
                into: Vec::new(),
            },
            "SAVEPOINT" | "MERGE" | "LOCK" | "SET" => {
                return Err(unsupported(&format!(
                    "{} in PL/SQL",
                    keyword.to_lowercase()
                )))
            }
            "EXECUTE" if self.is_kw_at(1, "IMMEDIATE") => {
                self.pos += 2;
                let sql = self.expr()?;
                let mut into = Vec::new();
                if self.eat_kw("INTO") {
                    into.push(target(self.returning_target()?)?);
                    while self.eat_sym(",") {
                        into.push(target(self.returning_target()?)?);
                    }
                }
                let mut using = Vec::new();
                if self.eat_kw("USING") {
                    loop {
                        if self.is_kw("OUT") || (self.is_kw("IN") && self.is_kw_at(1, "OUT")) {
                            return Err(unsupported("OUT binds in EXECUTE IMMEDIATE"));
                        }
                        self.eat_kw("IN");
                        using.push(self.expr()?);
                        if !self.eat_sym(",") {
                            break;
                        }
                    }
                }
                if self.is_kw("RETURNING") || self.is_kw("RETURN") {
                    return Err(unsupported("RETURNING in EXECUTE IMMEDIATE"));
                }
                Stmt::ExecuteImmediate { sql, into, using }
            }
            "OPEN" => {
                self.advance();
                let cursor = self.ident()?;
                if self.is_kw("FOR") {
                    return Err(unsupported("REF CURSOR"));
                }
                let args = self.args()?.into_iter().map(|a| a.value).collect();
                Stmt::Open { cursor, args }
            }
            "FETCH" => {
                self.advance();
                let cursor = self.ident()?;
                if self.is_kw("BULK") {
                    return Err(unsupported("BULK COLLECT"));
                }
                self.expect_kw("INTO")?;
                let mut into = vec![target(self.returning_target()?)?];
                while self.eat_sym(",") {
                    into.push(target(self.returning_target()?)?);
                }
                Stmt::Fetch { cursor, into }
            }
            "CLOSE" => {
                self.advance();
                Stmt::Close {
                    cursor: self.ident()?,
                }
            }
            "GOTO" | "FORALL" | "PIPE" => {
                return Err(unsupported(&keyword.to_lowercase()));
            }
            _ => {
                // An assignment or a procedure call.
                if let Tok::Bind(_) = tok {
                    let t = target(self.primary()?)?;
                    if !self.eat_sym(":=") {
                        return Err(expected(":="));
                    }
                    Stmt::Assign {
                        target: t,
                        value: self.expr()?,
                    }
                } else {
                    let name = self.dotted_name()?;
                    if self.eat_sym(":=") {
                        let t = match name.split_once('.') {
                            None => Target::Var(name),
                            Some((rec, field)) if !field.contains('.') => {
                                Target::Field(rec.to_string(), field.to_string())
                            }
                            _ => return Err(unsupported("assignment to a package variable")),
                        };
                        Stmt::Assign {
                            target: t,
                            value: self.expr()?,
                        }
                    } else {
                        let args = self.args()?;
                        Stmt::Call { name, args }
                    }
                }
            }
        };
        self.end_semicolon()?;
        Ok(stmt)
    }

    /// `LOOP statements END LOOP [label]`
    fn loop_body(&mut self) -> Result<Vec<Stmt>, OraError> {
        if !self.eat_kw("LOOP") {
            return Err(expected("LOOP"));
        }
        let body = self.statements()?;
        self.expect_kw("END")?;
        self.expect_kw("LOOP")?;
        if self.is_ident_at(0) {
            self.advance();
        }
        Ok(body)
    }
}

/// Turns an INTO target parsed as an expression into an assignment target.
fn target(e: Expr) -> Result<Target, OraError> {
    match e {
        Expr::Bind(i) => Ok(Target::Bind(i)),
        Expr::Column { table: None, name } => Ok(Target::Var(name)),
        Expr::Column {
            table: Some(rec),
            name,
        } => Ok(Target::Field(rec, name)),
        _ => Err(pls(
            103,
            "Encountered an unexpected symbol when expecting a variable",
        )),
    }
}
