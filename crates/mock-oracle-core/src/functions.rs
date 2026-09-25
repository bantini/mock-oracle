//! Built-in scalar SQL functions.

use std::cmp::Ordering;

use bigdecimal::{BigDecimal, RoundingMode, ToPrimitive};
use chrono::{Datelike, NaiveDateTime, Timelike};

use crate::eval::{round_number, Ex};
use crate::value::compare;
use crate::{datetime, OraError, Value};

fn arg_count() -> OraError {
    OraError::new(909, "invalid number of arguments")
}

fn check_args(args: &[Value], min: usize, max: usize) -> Result<(), OraError> {
    if args.len() < min || args.len() > max {
        Err(arg_count())
    } else {
        Ok(())
    }
}

fn num(v: &Value) -> Result<Option<BigDecimal>, OraError> {
    v.to_number()
}

fn int(v: &Value) -> Result<Option<i64>, OraError> {
    Ok(num(v)?.map(|n| {
        n.with_scale_round(0, RoundingMode::Down)
            .to_i64()
            .unwrap_or(i64::MAX)
    }))
}

fn from_f64(f: f64) -> Result<Value, OraError> {
    if !f.is_finite() {
        return Err(OraError::new(1426, "numeric overflow"));
    }
    let n: BigDecimal = f
        .to_string()
        .parse()
        .map_err(|_| OraError::new(1426, "numeric overflow"))?;
    Ok(Value::Number(round_number(n)))
}

impl Ex<'_> {
    fn text(&self, v: &Value) -> Option<String> {
        self.to_text(v)
    }

    fn date(&self, v: &Value) -> Result<Option<NaiveDateTime>, OraError> {
        v.to_datetime(&self.env.nls_date_format)
    }

    pub fn call(&self, name: &str, args: Vec<Value>) -> Result<Value, OraError> {
        let a = &args;
        let null = Value::Null;
        let get = |i: usize| a.get(i).unwrap_or(&null);
        let string_fn = |f: &dyn Fn(String) -> String| -> Result<Value, OraError> {
            check_args(a, 1, 1)?;
            Ok(self
                .text(&a[0])
                .map_or(Value::Null, |s| Value::varchar(f(s))))
        };
        match name {
            // ---- NULL handling ----
            "NVL" => {
                check_args(a, 2, 2)?;
                Ok(if a[0].is_null() {
                    a[1].clone()
                } else {
                    a[0].clone()
                })
            }
            "NVL2" => {
                check_args(a, 3, 3)?;
                Ok(if a[0].is_null() {
                    a[2].clone()
                } else {
                    a[1].clone()
                })
            }
            "COALESCE" => {
                check_args(a, 2, usize::MAX)?;
                Ok(args
                    .into_iter()
                    .find(|v| !v.is_null())
                    .unwrap_or(Value::Null))
            }
            "NULLIF" => {
                check_args(a, 2, 2)?;
                Ok(match compare(&a[0], &a[1], &self.env.nls_date_format)? {
                    Some(Ordering::Equal) => Value::Null,
                    _ => a[0].clone(),
                })
            }
            "DECODE" => {
                check_args(a, 3, 255)?;
                let subject = &a[0];
                let mut i = 1;
                while i + 1 < a.len() {
                    let hit = match (subject.is_null(), a[i].is_null()) {
                        (true, true) => true,
                        (false, false) => {
                            compare(subject, &a[i], &self.env.nls_date_format)?
                                == Some(Ordering::Equal)
                        }
                        _ => false,
                    };
                    if hit {
                        return Ok(a[i + 1].clone());
                    }
                    i += 2;
                }
                Ok(if i < a.len() {
                    a[i].clone()
                } else {
                    Value::Null
                })
            }

            // ---- strings ----
            "UPPER" => string_fn(&|s| s.to_uppercase()),
            "LOWER" => string_fn(&|s| s.to_lowercase()),
            "INITCAP" => string_fn(&|s| {
                let mut out = String::with_capacity(s.len());
                let mut start = true;
                for c in s.chars() {
                    if c.is_alphanumeric() {
                        if start {
                            out.extend(c.to_uppercase());
                        } else {
                            out.extend(c.to_lowercase());
                        }
                        start = false;
                    } else {
                        out.push(c);
                        start = true;
                    }
                }
                out
            }),
            "LENGTH" | "LENGTHB" => {
                check_args(a, 1, 1)?;
                Ok(self.text(&a[0]).map_or(Value::Null, |s| {
                    Value::number(if name == "LENGTH" {
                        s.chars().count()
                    } else {
                        s.len()
                    } as i64)
                }))
            }
            "SUBSTR" => {
                check_args(a, 2, 3)?;
                let (Some(s), Some(pos)) = (self.text(&a[0]), int(&a[1])?) else {
                    return Ok(Value::Null);
                };
                let chars: Vec<char> = s.chars().collect();
                let len = chars.len() as i64;
                let start = if pos > 0 {
                    pos - 1
                } else if pos == 0 {
                    0
                } else {
                    len + pos
                };
                if start < 0 || start >= len {
                    return Ok(Value::Null);
                }
                let count = match a.get(2) {
                    Some(v) => match int(v)? {
                        Some(n) if n >= 1 => n,
                        _ => return Ok(Value::Null),
                    },
                    None => len,
                };
                let end = (start + count).min(len);
                Ok(Value::varchar(
                    chars[start as usize..end as usize]
                        .iter()
                        .collect::<String>(),
                ))
            }
            "INSTR" => {
                check_args(a, 2, 4)?;
                let (Some(s), Some(sub)) = (self.text(&a[0]), self.text(&a[1])) else {
                    return Ok(Value::Null);
                };
                let pos = if a.len() > 2 { int(&a[2])? } else { Some(1) };
                let occ = if a.len() > 3 { int(&a[3])? } else { Some(1) };
                let (Some(pos), Some(occ)) = (pos, occ) else {
                    return Ok(Value::Null);
                };
                if pos == 0 {
                    return Ok(Value::number(0));
                }
                if occ < 1 {
                    return Err(OraError::new(
                        1428,
                        format!("argument '{occ}' is out of range"),
                    ));
                }
                let hay: Vec<char> = s.chars().collect();
                let needle: Vec<char> = sub.chars().collect();
                let n = hay.len() as i64;
                let m = needle.len() as i64;
                let matches_at = |i: i64| {
                    i >= 0 && i + m <= n && hay[i as usize..(i + m) as usize] == needle[..]
                };
                let mut found = 0;
                if pos > 0 {
                    let mut i = pos - 1;
                    while i + m <= n {
                        if matches_at(i) {
                            found += 1;
                            if found == occ {
                                return Ok(Value::number(i + 1));
                            }
                        }
                        i += 1;
                    }
                } else {
                    let mut i = n + pos - m + 1;
                    while i >= 0 {
                        if matches_at(i) {
                            found += 1;
                            if found == occ {
                                return Ok(Value::number(i + 1));
                            }
                        }
                        i -= 1;
                    }
                }
                Ok(Value::number(0))
            }
            "REPLACE" => {
                check_args(a, 2, 3)?;
                let Some(s) = self.text(&a[0]) else {
                    return Ok(Value::Null);
                };
                let Some(from) = self.text(&a[1]) else {
                    return Ok(Value::varchar(s));
                };
                let to = a.get(2).and_then(|v| self.text(v)).unwrap_or_default();
                Ok(Value::varchar(s.replace(&from, &to)))
            }
            "TRANSLATE" => {
                check_args(a, 3, 3)?;
                let (Some(s), Some(from), Some(to)) =
                    (self.text(&a[0]), self.text(&a[1]), self.text(&a[2]))
                else {
                    return Ok(Value::Null);
                };
                let from: Vec<char> = from.chars().collect();
                let to: Vec<char> = to.chars().collect();
                Ok(Value::varchar(
                    s.chars()
                        .filter_map(|c| match from.iter().position(|f| *f == c) {
                            Some(i) => to.get(i).copied(),
                            None => Some(c),
                        })
                        .collect::<String>(),
                ))
            }
            "TRIM" => {
                // Normalised by the parser to TRIM(mode, chars, string).
                let (Some(mode), Some(set), Some(s)) =
                    (self.text(get(0)), self.text(get(1)), self.text(get(2)))
                else {
                    return Ok(Value::Null);
                };
                if set.chars().count() != 1 {
                    return Err(OraError::new(
                        30001,
                        "trim set should have only one character",
                    ));
                }
                let c = set.chars().next().unwrap();
                Ok(Value::varchar(match mode.as_str() {
                    "LEADING" => s.trim_start_matches(c),
                    "TRAILING" => s.trim_end_matches(c),
                    _ => s.trim_matches(c),
                }))
            }
            "LTRIM" | "RTRIM" => {
                check_args(a, 1, 2)?;
                let Some(s) = self.text(&a[0]) else {
                    return Ok(Value::Null);
                };
                let set: Vec<char> = match a.get(1) {
                    Some(v) => match self.text(v) {
                        Some(t) => t.chars().collect(),
                        None => return Ok(Value::Null),
                    },
                    None => vec![' '],
                };
                Ok(Value::varchar(if name == "LTRIM" {
                    s.trim_start_matches(&set[..])
                } else {
                    s.trim_end_matches(&set[..])
                }))
            }
            "LPAD" | "RPAD" => {
                check_args(a, 2, 3)?;
                let (Some(s), Some(n)) = (self.text(&a[0]), int(&a[1])?) else {
                    return Ok(Value::Null);
                };
                let pad = match a.get(2) {
                    Some(v) => match self.text(v) {
                        Some(p) => p,
                        None => return Ok(Value::Null),
                    },
                    None => " ".into(),
                };
                if n <= 0 {
                    return Ok(Value::Null);
                }
                let n = n as usize;
                let chars: Vec<char> = s.chars().collect();
                if chars.len() >= n {
                    return Ok(Value::varchar(chars[..n].iter().collect::<String>()));
                }
                let fill: String = pad.chars().cycle().take(n - chars.len()).collect();
                Ok(Value::varchar(if name == "LPAD" {
                    fill + &s
                } else {
                    s + &fill
                }))
            }
            "CONCAT" => {
                check_args(a, 2, 2)?;
                Ok(Value::varchar(
                    self.text(&a[0]).unwrap_or_default() + &self.text(&a[1]).unwrap_or_default(),
                ))
            }
            "CHR" => {
                check_args(a, 1, 1)?;
                Ok(match int(&a[0])? {
                    Some(n) => Value::varchar(
                        char::from_u32(n as u32)
                            .map(String::from)
                            .unwrap_or_default(),
                    ),
                    None => Value::Null,
                })
            }
            "ASCII" => {
                check_args(a, 1, 1)?;
                Ok(self
                    .text(&a[0])
                    .and_then(|s| s.chars().next())
                    .map_or(Value::Null, |c| Value::number(c as i64)))
            }
            "REVERSE" => string_fn(&|s| s.chars().rev().collect()),

            // ---- numbers ----
            "ABS" | "SIGN" | "CEIL" | "FLOOR" | "SQRT" | "EXP" | "LN" => {
                check_args(a, 1, 1)?;
                let Some(n) = num(&a[0])? else {
                    return Ok(Value::Null);
                };
                let zero = BigDecimal::from(0);
                Ok(match name {
                    "ABS" => Value::Number(n.abs()),
                    "SIGN" => Value::number(match n.cmp(&zero) {
                        Ordering::Less => -1,
                        Ordering::Equal => 0,
                        Ordering::Greater => 1,
                    }),
                    "CEIL" => Value::Number(n.with_scale_round(0, RoundingMode::Ceiling)),
                    "FLOOR" => Value::Number(n.with_scale_round(0, RoundingMode::Floor)),
                    "SQRT" => {
                        if n < zero {
                            return Err(OraError::new(
                                1428,
                                format!("argument '{}' is out of range", crate::format_number(&n)),
                            ));
                        }
                        Value::Number(round_number(n.sqrt().unwrap_or_default()))
                    }
                    "EXP" => from_f64(n.to_f64().unwrap_or(f64::NAN).exp())?,
                    _ => {
                        if n <= zero {
                            return Err(OraError::new(
                                1428,
                                format!("argument '{}' is out of range", crate::format_number(&n)),
                            ));
                        }
                        from_f64(n.to_f64().unwrap_or(f64::NAN).ln())?
                    }
                })
            }
            "MOD" | "REMAINDER" | "POWER" | "LOG" => {
                check_args(a, 2, 2)?;
                let (Some(x), Some(y)) = (num(&a[0])?, num(&a[1])?) else {
                    return Ok(Value::Null);
                };
                let zero = BigDecimal::from(0);
                Ok(match name {
                    "MOD" => {
                        if y == zero {
                            Value::Number(x)
                        } else {
                            let q = (&x / &y).with_scale_round(0, RoundingMode::Down);
                            Value::Number(x - q * y)
                        }
                    }
                    "REMAINDER" => {
                        if y == zero {
                            return Err(OraError::new(1476, "divisor is equal to zero"));
                        }
                        let q = (&x / &y).with_scale_round(0, RoundingMode::HalfEven);
                        Value::Number(x - q * y)
                    }
                    "POWER" => {
                        if y.is_integer() && y.abs() <= 1000 {
                            let e = y.to_i64().unwrap();
                            let mut r = BigDecimal::from(1);
                            for _ in 0..e.abs() {
                                r = round_number(r * &x);
                            }
                            if e < 0 {
                                if r == zero {
                                    return Err(OraError::new(1476, "divisor is equal to zero"));
                                }
                                r = round_number(BigDecimal::from(1) / r);
                            }
                            Value::Number(r)
                        } else {
                            from_f64(
                                x.to_f64()
                                    .unwrap_or(f64::NAN)
                                    .powf(y.to_f64().unwrap_or(f64::NAN)),
                            )?
                        }
                    }
                    _ => from_f64(
                        y.to_f64()
                            .unwrap_or(f64::NAN)
                            .log(x.to_f64().unwrap_or(f64::NAN)),
                    )?,
                })
            }
            "ROUND" | "TRUNC" => {
                check_args(a, 1, 2)?;
                match &a[0] {
                    Value::Null => Ok(Value::Null),
                    Value::Date(d) | Value::Timestamp(d) => {
                        let unit = match a.get(1) {
                            Some(v) => match self.text(v) {
                                Some(u) => u,
                                None => return Ok(Value::Null),
                            },
                            None => "DD".into(),
                        };
                        let d = if name == "ROUND" {
                            round_date(d, &unit)?
                        } else {
                            datetime::trunc(d, &unit)?
                        };
                        Ok(Value::Date(d))
                    }
                    v => {
                        let Some(n) = num(v)? else {
                            return Ok(Value::Null);
                        };
                        let places = match a.get(1) {
                            Some(p) => match int(p)? {
                                Some(p) => p,
                                None => return Ok(Value::Null),
                            },
                            None => 0,
                        };
                        let mode = if name == "ROUND" {
                            RoundingMode::HalfUp
                        } else {
                            RoundingMode::Down
                        };
                        Ok(Value::Number(n.with_scale_round(places, mode).normalized()))
                    }
                }
            }
            "GREATEST" | "LEAST" => {
                check_args(a, 1, 255)?;
                if a.iter().any(Value::is_null) {
                    return Ok(Value::Null);
                }
                let mut best = a[0].clone();
                for v in &a[1..] {
                    let ord =
                        compare(v, &best, &self.env.nls_date_format)?.unwrap_or(Ordering::Equal);
                    if (name == "GREATEST" && ord == Ordering::Greater)
                        || (name == "LEAST" && ord == Ordering::Less)
                    {
                        best = v.clone();
                    }
                }
                Ok(best)
            }

            // ---- conversion ----
            "TO_CHAR" => {
                check_args(a, 1, 3)?;
                let fmt = match a.get(1) {
                    Some(f) => match self.text(f) {
                        Some(f) => Some(f),
                        None => return Ok(Value::Null),
                    },
                    None => None,
                };
                Ok(match (&a[0], fmt) {
                    (Value::Null, _) => Value::Null,
                    (Value::Date(d), f) => Value::varchar(datetime::format(
                        d,
                        f.as_deref().unwrap_or(&self.env.nls_date_format),
                    )),
                    (Value::Timestamp(d), f) => Value::varchar(datetime::format(
                        d,
                        f.as_deref().unwrap_or(&self.env.nls_timestamp_format),
                    )),
                    (v, None) => Value::varchar(self.text(v).unwrap_or_default()),
                    (v, Some(f)) => {
                        let n = num(v)?.unwrap();
                        Value::varchar(format_number_with(&n, &f)?)
                    }
                })
            }
            "TO_NUMBER" => {
                check_args(a, 1, 3)?;
                let Some(s) = self.text(&a[0]) else {
                    return Ok(Value::Null);
                };
                let s = if a.len() > 1 {
                    s.replace([',', '$'], "")
                } else {
                    s
                };
                Ok(Value::Number(
                    Value::varchar(s).to_number()?.unwrap_or_default(),
                ))
            }
            "TO_DATE" | "TO_TIMESTAMP" => {
                check_args(a, 1, 3)?;
                let Some(s) = self.text(&a[0]) else {
                    return Ok(Value::Null);
                };
                let default = if name == "TO_DATE" {
                    &self.env.nls_date_format
                } else {
                    &self.env.nls_timestamp_format
                };
                let fmt = match a.get(1) {
                    Some(f) => match self.text(f) {
                        Some(f) => f,
                        None => return Ok(Value::Null),
                    },
                    None => default.clone(),
                };
                let d = datetime::parse(&s, &fmt)?;
                Ok(if name == "TO_DATE" {
                    Value::Date(d.with_nanosecond(0).unwrap())
                } else {
                    Value::Timestamp(d)
                })
            }
            "CAST" => {
                let target = self.text(get(1)).unwrap_or_default();
                let v = a[0].clone();
                if v.is_null() {
                    return Ok(Value::Null);
                }
                match target.as_str() {
                    "NUMBER" => Ok(Value::Number(match &v {
                        Value::Varchar2(_) | Value::Number(_) => v.to_number()?.unwrap(),
                        _ => return Err(OraError::inconsistent("NUMBER", "DATE")),
                    })),
                    "DATE" => Ok(Value::Date(
                        self.date(&v)?.unwrap().with_nanosecond(0).unwrap(),
                    )),
                    "TIMESTAMP" => Ok(Value::Timestamp(
                        v.to_datetime(&self.env.nls_timestamp_format)?.unwrap(),
                    )),
                    t => {
                        let max: usize = t
                            .strip_prefix("VARCHAR2:")
                            .and_then(|n| n.parse().ok())
                            .unwrap_or(4000);
                        let s = self.text(&v).unwrap_or_default();
                        if s.len() > max {
                            return Err(OraError::new(25137, "Data value out of range"));
                        }
                        Ok(Value::varchar(s))
                    }
                }
            }

            // ---- dates ----
            "SYSDATE" => Ok(Value::Date(self.env.now_local.with_nanosecond(0).unwrap())),
            "SYSTIMESTAMP" => Ok(Value::Timestamp(truncate_micros(self.env.now_local))),
            "CURRENT_DATE" => Ok(Value::Date(
                self.env.session_now().with_nanosecond(0).unwrap(),
            )),
            "CURRENT_TIMESTAMP" | "LOCALTIMESTAMP" => {
                Ok(Value::Timestamp(truncate_micros(self.env.session_now())))
            }
            "USER" => Ok(Value::varchar(self.env.user.clone())),
            "ADD_MONTHS" => {
                check_args(a, 2, 2)?;
                let (Some(d), Some(n)) = (self.date(&a[0])?, int(&a[1])?) else {
                    return Ok(Value::Null);
                };
                Ok(Value::Date(datetime::add_months(
                    &d.with_nanosecond(0).unwrap(),
                    n as i32,
                )))
            }
            "LAST_DAY" => {
                check_args(a, 1, 1)?;
                let Some(d) = self.date(&a[0])? else {
                    return Ok(Value::Null);
                };
                let day = datetime::last_day_of(d.year(), d.month());
                Ok(Value::Date(
                    d.with_day(day).unwrap().with_nanosecond(0).unwrap(),
                ))
            }
            "MONTHS_BETWEEN" => {
                check_args(a, 2, 2)?;
                let (Some(x), Some(y)) = (self.date(&a[0])?, self.date(&a[1])?) else {
                    return Ok(Value::Null);
                };
                let whole = (x.year() - y.year()) as i64 * 12 + x.month() as i64 - y.month() as i64;
                let both_last = x.day() == datetime::last_day_of(x.year(), x.month())
                    && y.day() == datetime::last_day_of(y.year(), y.month());
                if x.day() == y.day() || both_last {
                    return Ok(Value::number(whole));
                }
                let secs = |d: &NaiveDateTime| {
                    (d.day() as i64 - 1) * 86_400 + d.num_seconds_from_midnight() as i64
                };
                let frac = BigDecimal::from(secs(&x) - secs(&y)) / BigDecimal::from(31 * 86_400);
                Ok(Value::Number(round_number(BigDecimal::from(whole) + frac)))
            }
            "EXTRACT" => {
                let field = self.text(get(0)).unwrap_or_default();
                let Some(d) = self.date(get(1))? else {
                    return Ok(Value::Null);
                };
                Ok(match field.as_str() {
                    "YEAR" => Value::number(d.year()),
                    "MONTH" => Value::number(d.month()),
                    "DAY" => Value::number(d.day()),
                    "HOUR" => Value::number(d.hour()),
                    "MINUTE" => Value::number(d.minute()),
                    "SECOND" => {
                        let n = BigDecimal::from(d.second())
                            + BigDecimal::from(d.nanosecond()) / BigDecimal::from(1_000_000_000);
                        Value::Number(n.normalized())
                    }
                    _ => return Err(OraError::new(907, "missing right parenthesis")),
                })
            }
            "SYS_GUID" => Err(OraError::new(3001, "unimplemented feature: SYS_GUID")),
            _ => Err(OraError::invalid_identifier(&format!("\"{name}\""))),
        }
    }
}

fn truncate_micros(d: NaiveDateTime) -> NaiveDateTime {
    d.with_nanosecond(d.nanosecond() / 1000 * 1000).unwrap_or(d)
}

fn round_date(d: &NaiveDateTime, unit: &str) -> Result<NaiveDateTime, OraError> {
    match unit.to_uppercase().as_str() {
        "DD" | "DDD" | "J" => datetime::trunc(&(*d + chrono::Duration::hours(12)), "DD"),
        "HH" | "HH24" => datetime::trunc(&(*d + chrono::Duration::minutes(30)), "HH"),
        "MI" => datetime::trunc(&(*d + chrono::Duration::seconds(30)), "MI"),
        "MM" | "MON" | "MONTH" => {
            let start = datetime::trunc(d, "MM")?;
            Ok(if d.day() >= 16 {
                datetime::add_months(&start, 1)
            } else {
                start
            })
        }
        "YYYY" | "YEAR" | "Y" | "YY" | "YYY" => {
            let start = datetime::trunc(d, "YYYY")?;
            Ok(if d.month() >= 7 {
                datetime::add_months(&start, 12)
            } else {
                start
            })
        }
        _ => datetime::trunc(d, unit),
    }
}

/// TO_CHAR(number, format) for the common format elements: 9 0 . D , G $ S MI FM.
pub fn format_number_with(n: &BigDecimal, fmt: &str) -> Result<String, OraError> {
    let invalid = || OraError::new(1481, "invalid number format model");
    let mut f = fmt.to_uppercase();
    let fm = f.starts_with("FM");
    if fm {
        f = f[2..].to_string();
    }
    let dollar = f.contains('$');
    let leading_sign = f.starts_with('S');
    let trailing_mi = f.ends_with("MI");
    let trailing_s = !leading_sign && f.ends_with('S');
    let core: String = f
        .chars()
        .filter(|c| !matches!(c, '$' | 'S'))
        .collect::<String>()
        .replace("MI", "");
    let (int_fmt, frac_fmt) = match core.find(['.', 'D']) {
        Some(i) => (&core[..i], Some(&core[i + 1..])),
        None => (&core[..], None),
    };
    if !int_fmt.chars().all(|c| matches!(c, '9' | '0' | ',' | 'G'))
        || !frac_fmt
            .unwrap_or("")
            .chars()
            .all(|c| matches!(c, '9' | '0'))
    {
        return Err(invalid());
    }
    let frac_digits = frac_fmt.map_or(0, |s| s.len());
    let rounded = n.with_scale_round(frac_digits as i64, RoundingMode::HalfUp);
    let negative = rounded < 0;
    let abs = rounded.abs();
    let plain = abs.with_scale(frac_digits as i64).to_plain_string();
    let (int_digits, frac_part) = match plain.find('.') {
        Some(i) => (&plain[..i], &plain[i + 1..]),
        None => (&plain[..], ""),
    };
    let int_digits = if int_digits == "0" { "" } else { int_digits };
    let digit_slots = int_fmt.chars().filter(|c| matches!(c, '9' | '0')).count();
    let width = fmt.len() + 1;
    if int_digits.len() > digit_slots {
        return Ok("#".repeat(width));
    }
    // Fill the integer part from the right.
    let mut digits = int_digits.chars().rev();
    let mut out: Vec<char> = Vec::new();
    let mut placed_all = int_digits.is_empty();
    let first_zero = int_fmt.find('0');
    for (pos, c) in int_fmt.char_indices().rev() {
        match c {
            '9' | '0' => match digits.next() {
                Some(d) => {
                    out.push(d);
                    if digits.clone().next().is_none() {
                        placed_all = true;
                    }
                }
                None => {
                    // A leading 0 in the format forces zeros from there on.
                    if first_zero.is_some_and(|z| pos >= z) {
                        out.push('0');
                    } else {
                        out.push(' ');
                    }
                }
            },
            _ => {
                if !placed_all || first_zero.is_some_and(|z| pos > z) {
                    out.push(',');
                } else {
                    out.push(' ');
                }
            }
        }
    }
    // Oracle shows "0" for a zero integer part when there is no fraction.
    if int_digits.is_empty() && frac_fmt.is_none() && !out.contains(&'0') {
        if let Some(last) = out.first_mut() {
            *last = '0';
        }
    }
    out.reverse();
    let mut int_str: String = out.into_iter().collect();
    let lead = int_str.len() - int_str.trim_start().len();
    let body = int_str.split_off(lead);
    let mut frac_str = String::new();
    if let Some(ff) = frac_fmt {
        let mut frac: String = frac_part.to_string();
        if fm {
            // FM drops trailing zeros in '9' positions.
            let keep = ff.rfind('0').map_or(0, |i| i + 1);
            while frac.len() > keep && frac.ends_with('0') {
                frac.pop();
            }
        }
        frac_str = format!(".{frac}");
    }
    let currency = if dollar { "$" } else { "" };
    let sign_before = if leading_sign {
        if negative {
            "-"
        } else {
            "+"
        }
    } else if trailing_mi || trailing_s {
        ""
    } else if negative {
        "-"
    } else if fm {
        ""
    } else {
        " "
    };
    let sign_after = if trailing_mi {
        if negative {
            "-"
        } else if fm {
            ""
        } else {
            " "
        }
    } else if trailing_s {
        if negative {
            "-"
        } else {
            "+"
        }
    } else {
        ""
    };
    let padding = if fm { String::new() } else { " ".repeat(lead) };
    Ok(format!(
        "{padding}{sign_before}{currency}{body}{frac_str}{sign_after}"
    ))
}

#[cfg(test)]
mod tests {
    use super::format_number_with;
    use std::str::FromStr;

    fn f(n: &str, fmt: &str) -> String {
        format_number_with(&bigdecimal::BigDecimal::from_str(n).unwrap(), fmt).unwrap()
    }

    #[test]
    fn number_formats() {
        assert_eq!(f("1234.5", "9999.99"), " 1234.50");
        assert_eq!(f("1234.5", "9,999.99"), " 1,234.50");
        assert_eq!(f("0.5", "9.99"), "  .50");
        assert_eq!(f("0.5", "0.99"), " 0.50");
        assert_eq!(f("-12", "999"), " -12");
        assert_eq!(f("42", "FM999"), "42");
        assert_eq!(f("1.5", "FM999.99"), "1.5");
        assert_eq!(f("7", "000"), " 007");
        assert_eq!(f("12345", "999"), "####");
        assert_eq!(f("5", "$99.00"), "  $5.00");
        assert_eq!(f("0", "999"), "   0");
    }
}
