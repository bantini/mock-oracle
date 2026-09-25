use std::fmt;
use std::str::FromStr;

use bigdecimal::BigDecimal;

/// A SQL value. There is no empty string: Oracle treats `''` as NULL, and
/// [`Value::varchar`] enforces that.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Number(BigDecimal),
    Varchar2(String),
}

impl Value {
    /// Builds a VARCHAR2 value, turning the empty string into NULL as Oracle does.
    pub fn varchar(s: impl Into<String>) -> Self {
        let s = s.into();
        if s.is_empty() {
            Value::Null
        } else {
            Value::Varchar2(s)
        }
    }

    pub fn number(n: impl Into<BigDecimal>) -> Self {
        Value::Number(n.into())
    }

    /// Parses a decimal literal such as `42`, `-1.5` or `1e3`.
    pub fn parse_number(s: &str) -> Option<Self> {
        BigDecimal::from_str(s).ok().map(Value::Number)
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Number(n) => f.write_str(&format_number(n)),
            Value::Varchar2(s) => f.write_str(s),
        }
    }
}

/// Formats a number the way Oracle's default TO_CHAR does: no exponent, no
/// trailing fractional zeros, and no leading zero before the decimal point.
pub fn format_number(n: &BigDecimal) -> String {
    let n = n.normalized();
    let s = if n.fractional_digit_count() < 0 {
        n.with_scale(0).to_string()
    } else {
        n.to_plain_string()
    };
    if let Some(rest) = s.strip_prefix("0.") {
        format!(".{rest}")
    } else if let Some(rest) = s.strip_prefix("-0.") {
        format!("-.{rest}")
    } else {
        s
    }
}

/// The SQL type of a result column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    Number,
    /// VARCHAR2 with its maximum length in bytes. Length 0 means the column is
    /// always NULL (for example `SELECT NULL FROM DUAL`).
    Varchar2(u32),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_null() {
        assert_eq!(Value::varchar(""), Value::Null);
    }

    #[test]
    fn numbers_format_like_oracle() {
        let f = |s: &str| format_number(&BigDecimal::from_str(s).unwrap());
        assert_eq!(f("1"), "1");
        assert_eq!(f("1.50"), "1.5");
        assert_eq!(f("0.25"), ".25");
        assert_eq!(f("-0.25"), "-.25");
        assert_eq!(f("1e3"), "1000");
        assert_eq!(f("0"), "0");
    }
}
