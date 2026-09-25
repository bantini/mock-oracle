//! Tokenizer for Oracle SQL.

use crate::OraError;

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    /// Unquoted identifier or keyword, uppercased.
    Ident(String),
    /// `"Quoted"` identifier, case preserved.
    QuotedIdent(String),
    Number(String),
    Str(String),
    /// `:name` or `:1`.
    Bind(String),
    Symbol(&'static str),
}

#[derive(Debug, Clone)]
pub struct Token {
    pub tok: Tok,
    /// Byte offsets into the statement text.
    pub start: usize,
    pub end: usize,
}

const SYMBOLS: &[&str] = &[
    "||", "<=", ">=", "<>", "!=", "^=", "=>", "..", "<<", ">>", "**", "(", ")", ",", "+", "-", "*",
    "/", "=", "<", ">", ".", ";", "%",
];

pub fn tokenize(sql: &str) -> Result<Vec<Token>, OraError> {
    let bytes = sql.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
        } else if sql[i..].starts_with("--") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if sql[i..].starts_with("/*") {
            match sql[i + 2..].find("*/") {
                Some(end) => i += end + 4,
                None => i = bytes.len(),
            }
        } else if c == b'\'' {
            let mut s = String::new();
            i += 1;
            loop {
                match sql[i..].find('\'') {
                    None => {
                        return Err(OraError::new(1756, "quoted string not properly terminated"))
                    }
                    Some(q) => {
                        s.push_str(&sql[i..i + q]);
                        i += q + 1;
                        if bytes.get(i) == Some(&b'\'') {
                            s.push('\'');
                            i += 1;
                        } else {
                            break;
                        }
                    }
                }
            }
            tokens.push(Token {
                tok: Tok::Str(s),
                start,
                end: i,
            });
        } else if c == b'"' {
            match sql[i + 1..].find('"') {
                None => return Err(OraError::new(1740, "missing double quote in identifier")),
                Some(q) => {
                    let name = sql[i + 1..i + 1 + q].to_string();
                    if name.is_empty() {
                        return Err(OraError::new(1741, "illegal zero-length identifier"));
                    }
                    i += q + 2;
                    tokens.push(Token {
                        tok: Tok::QuotedIdent(name),
                        start,
                        end: i,
                    });
                }
            }
        } else if c.is_ascii_digit()
            || (c == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit))
        {
            // Digits and one decimal point; `1..10` is a PL/SQL range, not a number.
            let mut seen_point = false;
            while i < bytes.len()
                && (bytes[i].is_ascii_digit()
                    || (bytes[i] == b'.' && !seen_point && bytes.get(i + 1) != Some(&b'.')))
            {
                seen_point |= bytes[i] == b'.';
                i += 1;
            }
            if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
                let mut j = i + 1;
                if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
                    j += 1;
                }
                if j < bytes.len() && bytes[j].is_ascii_digit() {
                    i = j;
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            tokens.push(Token {
                tok: Tok::Number(sql[start..i].to_string()),
                start,
                end: i,
            });
        } else if sql[i..].starts_with(":=") {
            i += 2;
            tokens.push(Token {
                tok: Tok::Symbol(":="),
                start,
                end: i,
            });
        } else if c == b':' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            if i == start + 1 {
                return Err(OraError::new(911, "invalid character"));
            }
            tokens.push(Token {
                tok: Tok::Bind(sql[start + 1..i].to_uppercase()),
                start,
                end: i,
            });
        } else if c.is_ascii_alphabetic() || c >= 0x80 {
            while i < bytes.len()
                && (bytes[i].is_ascii_alphanumeric()
                    || matches!(bytes[i], b'_' | b'$' | b'#')
                    || bytes[i] >= 0x80)
            {
                i += 1;
            }
            tokens.push(Token {
                tok: Tok::Ident(sql[start..i].to_uppercase()),
                start,
                end: i,
            });
        } else if let Some(sym) = SYMBOLS.iter().find(|s| sql[i..].starts_with(**s)) {
            i += sym.len();
            tokens.push(Token {
                tok: Tok::Symbol(sym),
                start,
                end: i,
            });
        } else {
            return Err(OraError::new(911, "invalid character"));
        }
    }
    Ok(tokens)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(sql: &str) -> Vec<Tok> {
        tokenize(sql).unwrap().into_iter().map(|t| t.tok).collect()
    }

    #[test]
    fn tokenizes_select() {
        assert_eq!(
            toks("select 'it''s', 1.5e2, :id from dual -- note"),
            vec![
                Tok::Ident("SELECT".into()),
                Tok::Str("it's".into()),
                Tok::Symbol(","),
                Tok::Number("1.5e2".into()),
                Tok::Symbol(","),
                Tok::Bind("ID".into()),
                Tok::Ident("FROM".into()),
                Tok::Ident("DUAL".into()),
            ]
        );
    }

    #[test]
    fn tokenizes_plsql_symbols() {
        assert_eq!(
            toks("for i in 1..10 loop x := c%rowcount; p(a => 1.5); end loop"),
            vec![
                Tok::Ident("FOR".into()),
                Tok::Ident("I".into()),
                Tok::Ident("IN".into()),
                Tok::Number("1".into()),
                Tok::Symbol(".."),
                Tok::Number("10".into()),
                Tok::Ident("LOOP".into()),
                Tok::Ident("X".into()),
                Tok::Symbol(":="),
                Tok::Ident("C".into()),
                Tok::Symbol("%"),
                Tok::Ident("ROWCOUNT".into()),
                Tok::Symbol(";"),
                Tok::Ident("P".into()),
                Tok::Symbol("("),
                Tok::Ident("A".into()),
                Tok::Symbol("=>"),
                Tok::Number("1.5".into()),
                Tok::Symbol(")"),
                Tok::Symbol(";"),
                Tok::Ident("END".into()),
                Tok::Ident("LOOP".into()),
            ]
        );
    }

    #[test]
    fn unterminated_string_is_ora_01756() {
        assert_eq!(tokenize("select 'x from dual").unwrap_err().code, 1756);
    }
}
