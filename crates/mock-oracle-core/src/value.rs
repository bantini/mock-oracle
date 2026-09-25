use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use bigdecimal::BigDecimal;
use chrono::NaiveDateTime;

use crate::datetime;
use crate::OraError;

/// A SQL value. There is no empty string: Oracle treats `''` as NULL, and
/// [`Value::varchar`] enforces that.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Number(BigDecimal),
    Varchar2(String),
    /// Oracle DATE: date and time to the second, no time zone.
    Date(NaiveDateTime),
    /// Oracle TIMESTAMP: date and time with fractional seconds, no time zone.
    Timestamp(NaiveDateTime),
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

    /// Implicit conversion to NUMBER.
    pub fn to_number(&self) -> Result<Option<BigDecimal>, OraError> {
        match self {
            Value::Null => Ok(None),
            Value::Number(n) => Ok(Some(n.clone())),
            Value::Varchar2(s) => BigDecimal::from_str(s.trim())
                .map(Some)
                .map_err(|_| OraError::new(1722, "invalid number")),
            Value::Date(_) | Value::Timestamp(_) => Err(OraError::inconsistent("NUMBER", "DATE")),
        }
    }

    /// Implicit conversion to a date/time, using the default date format for strings.
    pub fn to_datetime(&self, nls_date_format: &str) -> Result<Option<NaiveDateTime>, OraError> {
        match self {
            Value::Null => Ok(None),
            Value::Date(d) | Value::Timestamp(d) => Ok(Some(*d)),
            Value::Varchar2(s) => datetime::parse(s, nls_date_format).map(Some),
            Value::Number(_) => Err(OraError::inconsistent("DATE", "NUMBER")),
        }
    }
}

impl fmt::Display for Value {
    /// The text Oracle produces when converting to VARCHAR2 with default formats.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => Ok(()),
            Value::Number(n) => f.write_str(&format_number(n)),
            Value::Varchar2(s) => f.write_str(s),
            Value::Date(d) => f.write_str(&datetime::format(d, datetime::DEFAULT_DATE_FORMAT)),
            Value::Timestamp(d) => {
                f.write_str(&datetime::format(d, datetime::DEFAULT_TIMESTAMP_FORMAT))
            }
        }
    }
}

/// Compares two non-NULL values the way Oracle does, converting strings to
/// numbers or dates when the other side is one. Returns `None` if either is NULL.
pub fn compare(a: &Value, b: &Value, nls_date_format: &str) -> Result<Option<Ordering>, OraError> {
    Ok(match (a, b) {
        (Value::Null, _) | (_, Value::Null) => None,
        // Blank-padded comparison, so CHAR columns match unpadded literals.
        (Value::Varchar2(x), Value::Varchar2(y)) => Some(
            x.trim_end_matches(' ')
                .as_bytes()
                .cmp(y.trim_end_matches(' ').as_bytes()),
        ),
        (Value::Number(_), _) | (_, Value::Number(_)) => {
            let (x, y) = (a.to_number()?.unwrap(), b.to_number()?.unwrap());
            Some(x.cmp(&y))
        }
        _ => {
            let x = a.to_datetime(nls_date_format)?.unwrap();
            let y = b.to_datetime(nls_date_format)?.unwrap();
            Some(x.cmp(&y))
        }
    })
}

/// A string that is equal for two values exactly when they are the same value, for
/// grouping, DISTINCT, set operations and unique indexes. NULLs compare equal here.
pub fn key_string<'a>(values: impl IntoIterator<Item = &'a Value>) -> String {
    let mut key = String::new();
    for v in values {
        match v {
            Value::Null => key.push('~'),
            Value::Number(n) => {
                key.push('n');
                key.push_str(&n.normalized().to_string());
            }
            Value::Varchar2(s) => {
                key.push('s');
                key.push_str(s);
            }
            Value::Date(d) | Value::Timestamp(d) => {
                key.push('d');
                key.push_str(
                    &d.and_utc()
                        .timestamp_nanos_opt()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
        key.push('\u{1}');
    }
    key
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

/// The SQL type of a result column or table column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlType {
    /// NUMBER(precision, scale). Precision 0 and scale -127 mean an unconstrained NUMBER.
    Number {
        precision: u8,
        scale: i8,
    },
    /// VARCHAR2 with its maximum length in bytes. Length 0 means the column is
    /// always NULL (for example `SELECT NULL FROM DUAL`).
    Varchar2(u32),
    /// Blank-padded CHAR with its length in bytes.
    Char(u32),
    Date,
    /// TIMESTAMP with its fractional-second precision.
    Timestamp(u8),
}

impl SqlType {
    pub const NUMBER: SqlType = SqlType::Number {
        precision: 0,
        scale: -127,
    };
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

    #[test]
    fn comparisons_convert_like_oracle() {
        let n = |s: &str| Value::parse_number(s).unwrap();
        let fmt = datetime::DEFAULT_DATE_FORMAT;
        assert_eq!(
            compare(&n("10"), &Value::varchar("9"), fmt).unwrap(),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare(&Value::varchar("10"), &Value::varchar("9"), fmt).unwrap(),
            Some(Ordering::Less)
        );
        assert_eq!(compare(&Value::Null, &n("1"), fmt).unwrap(), None);
        assert_eq!(
            compare(&n("1"), &Value::varchar("x"), fmt)
                .unwrap_err()
                .code,
            1722
        );
    }
}
