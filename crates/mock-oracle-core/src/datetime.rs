//! Oracle date format models (`YYYY-MM-DD HH24:MI:SS`, `DD-MON-RR`, ...) for
//! TO_CHAR and TO_DATE.

use chrono::{Datelike, NaiveDate, NaiveDateTime, Timelike};

use crate::OraError;

pub const DEFAULT_DATE_FORMAT: &str = "DD-MON-RR";
pub const DEFAULT_TIMESTAMP_FORMAT: &str = "DD-MON-RR HH.MI.SSXFF AM";

const MONTHS: [&str; 12] = [
    "JANUARY",
    "FEBRUARY",
    "MARCH",
    "APRIL",
    "MAY",
    "JUNE",
    "JULY",
    "AUGUST",
    "SEPTEMBER",
    "OCTOBER",
    "NOVEMBER",
    "DECEMBER",
];
const DAYS: [&str; 7] = [
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];

#[derive(Debug, Clone, PartialEq)]
enum Elem {
    Yyyy,
    Yy,
    Rr,
    Mm,
    Mon,
    Month,
    Dd,
    Ddd,
    D,
    Dy,
    Day,
    Hh24,
    Hh12,
    Mi,
    Ss,
    Ff(u8),
    Am,
    /// `X`: the local radix character.
    X,
    Fm,
    Literal(String),
}

/// Letter case of a format element, which Oracle mirrors in month/day names.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Case {
    Upper,
    Capital,
    Lower,
}

fn case_of(s: &str) -> Case {
    let mut chars = s.chars();
    let first = chars.next().is_some_and(|c| c.is_uppercase());
    let second = chars.next().is_some_and(|c| c.is_uppercase());
    match (first, second) {
        (true, true) => Case::Upper,
        (true, false) => Case::Capital,
        _ => Case::Lower,
    }
}

fn apply_case(s: &str, case: Case) -> String {
    match case {
        Case::Upper => s.to_uppercase(),
        Case::Lower => s.to_lowercase(),
        Case::Capital => {
            let lower = s.to_lowercase();
            let mut c = lower.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        }
    }
}

fn tokenize(fmt: &str) -> Result<Vec<(Elem, Case)>, OraError> {
    let mut out = Vec::new();
    let upper = fmt.to_uppercase();
    let mut i = 0;
    let bad = || OraError::new(1821, "date format not recognized");
    while i < fmt.len() {
        let rest = &upper[i..];
        let orig = &fmt[i..];
        let case = case_of(orig);
        if let Some(stripped) = orig.strip_prefix('"') {
            let end = stripped.find('"').ok_or_else(bad)?;
            out.push((Elem::Literal(stripped[..end].to_string()), case));
            i += end + 2;
            continue;
        }
        let c = rest.chars().next().unwrap();
        if matches!(c, '-' | '/' | ',' | '.' | ';' | ':' | ' ') {
            out.push((Elem::Literal(c.to_string()), case));
            i += 1;
            continue;
        }
        let table: &[(&str, Elem)] = &[
            ("YYYY", Elem::Yyyy),
            ("RRRR", Elem::Yyyy),
            ("YY", Elem::Yy),
            ("RR", Elem::Rr),
            ("MONTH", Elem::Month),
            ("MON", Elem::Mon),
            ("MM", Elem::Mm),
            ("DDD", Elem::Ddd),
            ("DD", Elem::Dd),
            ("DAY", Elem::Day),
            ("DY", Elem::Dy),
            ("D", Elem::D),
            ("HH24", Elem::Hh24),
            ("HH12", Elem::Hh12),
            ("HH", Elem::Hh12),
            ("MI", Elem::Mi),
            ("SS", Elem::Ss),
            ("AM", Elem::Am),
            ("PM", Elem::Am),
            ("A.M.", Elem::Am),
            ("P.M.", Elem::Am),
            ("FM", Elem::Fm),
            ("X", Elem::X),
        ];
        if let Some(rest_ff) = rest.strip_prefix("FF") {
            let digits = rest_ff
                .chars()
                .next()
                .filter(|c| c.is_ascii_digit() && *c != '0');
            let precision = digits.map_or(9, |d| d.to_digit(10).unwrap() as u8);
            i += 2 + usize::from(digits.is_some());
            out.push((Elem::Ff(precision), case));
            continue;
        }
        match table.iter().find(|(k, _)| rest.starts_with(k)) {
            Some((k, e)) => {
                out.push((e.clone(), case));
                i += k.len();
            }
            None => return Err(bad()),
        }
    }
    Ok(out)
}

/// Checks that a format model is valid (ORA-01821 otherwise).
pub fn validate_format(fmt: &str) -> Result<(), OraError> {
    tokenize(fmt).map(|_| ())
}

/// TO_CHAR(datetime, format).
pub fn format(d: &NaiveDateTime, fmt: &str) -> String {
    let Ok(elems) = tokenize(fmt) else {
        return d.to_string();
    };
    let mut fm = false;
    let mut out = String::new();
    let pad = |n: u32, width: usize, fm: bool| {
        if fm {
            n.to_string()
        } else {
            format!("{n:0width$}")
        }
    };
    for (e, case) in elems {
        match e {
            Elem::Yyyy => out.push_str(&pad(d.year() as u32, 4, fm)),
            Elem::Yy | Elem::Rr => out.push_str(&format!("{:02}", d.year() % 100)),
            Elem::Mm => out.push_str(&pad(d.month(), 2, fm)),
            Elem::Mon => out.push_str(&apply_case(&MONTHS[d.month0() as usize][..3], case)),
            Elem::Month => {
                let name = apply_case(MONTHS[d.month0() as usize], case);
                out.push_str(&if fm { name } else { format!("{name:<9}") });
            }
            Elem::Dd => out.push_str(&pad(d.day(), 2, fm)),
            Elem::Ddd => out.push_str(&pad(d.ordinal(), 3, fm)),
            Elem::D => out.push_str(&(d.weekday().num_days_from_sunday() + 1).to_string()),
            Elem::Dy => out.push_str(&apply_case(
                &DAYS[d.weekday().num_days_from_monday() as usize][..3],
                case,
            )),
            Elem::Day => {
                let name = apply_case(DAYS[d.weekday().num_days_from_monday() as usize], case);
                out.push_str(&if fm { name } else { format!("{name:<9}") });
            }
            Elem::Hh24 => out.push_str(&pad(d.hour(), 2, fm)),
            Elem::Hh12 => {
                let h = d.hour() % 12;
                out.push_str(&pad(if h == 0 { 12 } else { h }, 2, fm));
            }
            Elem::Mi => out.push_str(&pad(d.minute(), 2, fm)),
            Elem::Ss => out.push_str(&pad(d.second(), 2, fm)),
            Elem::Ff(p) => {
                let nanos = format!("{:09}", d.nanosecond() % 1_000_000_000);
                out.push_str(&nanos[..p as usize]);
            }
            Elem::Am => out.push_str(&apply_case(if d.hour() < 12 { "AM" } else { "PM" }, case)),
            Elem::X => out.push('.'),
            Elem::Fm => fm = !fm,
            Elem::Literal(s) => out.push_str(&s),
        }
    }
    out
}

/// TO_DATE(string, format). Oracle is lenient about separators and zero padding.
pub fn parse(s: &str, fmt: &str) -> Result<NaiveDateTime, OraError> {
    let elems = tokenize(fmt)?;
    let input = s.trim();
    let bytes = input.as_bytes();
    let mut i = 0;
    let (mut year, mut month, mut day) = (None, 1u32, 1u32);
    let (mut hour, mut minute, mut second, mut nanos) = (0u32, 0u32, 0u32, 0u32);
    let mut pm: Option<bool> = None;
    let mut hour12 = false;
    let no_match = || OraError::new(1861, "literal does not match format string");

    let number = |i: &mut usize, max: usize| -> Result<(u32, usize), OraError> {
        let start = *i;
        while *i < bytes.len() && *i - start < max && bytes[*i].is_ascii_digit() {
            *i += 1;
        }
        if *i == start {
            return Err(OraError::new(
                1858,
                "a non-numeric character was found where a numeric was expected",
            ));
        }
        Ok((input[start..*i].parse().unwrap(), *i - start))
    };

    for (e, _) in &elems {
        match e {
            Elem::Yyyy => year = Some(number(&mut i, 4)?.0 as i32),
            Elem::Yy => year = Some(2000 + number(&mut i, 2)?.0 as i32),
            Elem::Rr => {
                let (y, len) = number(&mut i, 4)?;
                year = Some(if len > 2 {
                    y as i32
                } else if y < 50 {
                    2000 + y as i32
                } else {
                    1900 + y as i32
                });
            }
            Elem::Mm => month = number(&mut i, 2)?.0,
            Elem::Mon | Elem::Month => {
                let rest = input[i..].to_uppercase();
                let idx = MONTHS
                    .iter()
                    .position(|m| rest.starts_with(m) || rest.starts_with(&m[..3]))
                    .ok_or_else(|| OraError::new(1843, "not a valid month"))?;
                i += if rest.starts_with(MONTHS[idx]) {
                    MONTHS[idx].len()
                } else {
                    3
                };
                month = idx as u32 + 1;
            }
            Elem::Dd => day = number(&mut i, 2)?.0,
            Elem::Ddd | Elem::D => {
                number(&mut i, 3)?;
            }
            Elem::Dy | Elem::Day => {
                let rest = input[i..].to_uppercase();
                let d = DAYS
                    .iter()
                    .find(|d| rest.starts_with(*d) || rest.starts_with(&d[..3]))
                    .ok_or_else(|| OraError::new(1846, "not a valid day of the week"))?;
                i += if rest.starts_with(d) { d.len() } else { 3 };
            }
            Elem::Hh24 => hour = number(&mut i, 2)?.0,
            Elem::Hh12 => {
                hour = number(&mut i, 2)?.0;
                hour12 = true;
            }
            Elem::Mi => minute = number(&mut i, 2)?.0,
            Elem::Ss => second = number(&mut i, 2)?.0,
            Elem::Ff(_) => {
                let (n, len) = number(&mut i, 9)?;
                nanos = n * 10u32.pow(9 - len as u32);
            }
            Elem::Am => {
                let rest = input[i..].to_uppercase();
                let (is_pm, len) = if rest.starts_with("PM") {
                    (true, 2)
                } else if rest.starts_with("AM") {
                    (false, 2)
                } else if rest.starts_with("P.M.") {
                    (true, 4)
                } else if rest.starts_with("A.M.") {
                    (false, 4)
                } else {
                    return Err(OraError::new(1855, "AM/A.M. or PM/P.M. required"));
                };
                pm = Some(is_pm);
                i += len;
            }
            Elem::X => {
                if bytes.get(i) == Some(&b'.') {
                    i += 1;
                }
            }
            Elem::Fm => {}
            Elem::Literal(lit) => {
                // Any punctuation matches any punctuation, as in Oracle's default (non-FX) mode.
                if lit.chars().all(|c| !c.is_alphanumeric()) {
                    while i < bytes.len() && !bytes[i].is_ascii_alphanumeric() {
                        i += 1;
                    }
                } else if input[i..].to_uppercase().starts_with(&lit.to_uppercase()) {
                    i += lit.len();
                } else {
                    return Err(no_match());
                }
            }
        }
    }
    if i < bytes.len() {
        return Err(OraError::new(
            1830,
            "date format picture ends before converting entire input string",
        ));
    }
    if hour12 || pm.is_some() {
        if !(1..=12).contains(&hour) {
            return Err(OraError::new(1849, "hour must be between 1 and 12"));
        }
        hour %= 12;
        if pm == Some(true) {
            hour += 12;
        }
    }
    if hour > 23 {
        return Err(OraError::new(1850, "hour must be between 0 and 23"));
    }
    if minute > 59 {
        return Err(OraError::new(1851, "minutes must be between 0 and 59"));
    }
    if second > 59 {
        return Err(OraError::new(1852, "seconds must be between 0 and 59"));
    }
    if !(1..=12).contains(&month) {
        return Err(OraError::new(1843, "not a valid month"));
    }
    let year = year.unwrap_or_else(|| chrono::Local::now().year());
    let date = NaiveDate::from_ymd_opt(year, month, day)
        .ok_or_else(|| OraError::new(1839, "date not valid for month specified"))?;
    Ok(date.and_hms_nano_opt(hour, minute, second, nanos).unwrap())
}

/// Adds months the way ADD_MONTHS does: the last day of a month maps to the last day.
pub fn add_months(d: &NaiveDateTime, months: i32) -> NaiveDateTime {
    let total = d.year() * 12 + d.month0() as i32 + months;
    let (year, month) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let last = last_day_of(year, month);
    let day = if d.day() == last_day_of(d.year(), d.month()) {
        last
    } else {
        d.day().min(last)
    };
    NaiveDate::from_ymd_opt(year, month, day)
        .unwrap()
        .and_time(d.time())
}

pub fn last_day_of(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .unwrap()
        .pred_opt()
        .unwrap()
        .day()
}

/// TRUNC(date, format): truncates to the unit the format names.
pub fn trunc(d: &NaiveDateTime, unit: &str) -> Result<NaiveDateTime, OraError> {
    let date = d.date();
    let t = |date: NaiveDate| date.and_hms_opt(0, 0, 0).unwrap();
    Ok(match unit.to_uppercase().as_str() {
        "DD" | "DDD" | "J" => t(date),
        "MM" | "MON" | "MONTH" | "RM" => {
            t(NaiveDate::from_ymd_opt(date.year(), date.month(), 1).unwrap())
        }
        "YYYY" | "YYY" | "YY" | "Y" | "YEAR" | "SYYYY" | "RRRR" | "RR" => {
            t(NaiveDate::from_ymd_opt(date.year(), 1, 1).unwrap())
        }
        "Q" => t(NaiveDate::from_ymd_opt(date.year(), (date.month0() / 3) * 3 + 1, 1).unwrap()),
        "HH" | "HH12" | "HH24" => date.and_hms_opt(d.hour(), 0, 0).unwrap(),
        "MI" => date.and_hms_opt(d.hour(), d.minute(), 0).unwrap(),
        "D" | "DY" | "DAY" => {
            t(date - chrono::Duration::days(date.weekday().num_days_from_sunday() as i64))
        }
        "IW" => t(date - chrono::Duration::days(date.weekday().num_days_from_monday() as i64)),
        _ => return Err(OraError::new(1898, "too many precision specifiers")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dt(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f").unwrap()
    }

    #[test]
    fn formats() {
        let d = dt("2024-03-05 14:07:09.123456");
        assert_eq!(format(&d, "YYYY-MM-DD HH24:MI:SS"), "2024-03-05 14:07:09");
        assert_eq!(format(&d, "DD-MON-RR"), "05-MAR-24");
        assert_eq!(format(&d, "fmDD Month YYYY"), "5 March 2024");
        assert_eq!(format(&d, "HH:MI AM"), "02:07 PM");
        assert_eq!(format(&d, "Dy, DD.MM.YYYY"), "Tue, 05.03.2024");
        assert_eq!(format(&d, "HH24:MI:SS.FF3"), "14:07:09.123");
        assert_eq!(format(&d, "YYYY\"Q\"MM"), "2024Q03");
    }

    #[test]
    fn parses() {
        assert_eq!(
            parse("2024-03-05", "YYYY-MM-DD").unwrap(),
            dt("2024-03-05 00:00:00")
        );
        assert_eq!(
            parse("05-MAR-24", "DD-MON-RR").unwrap(),
            dt("2024-03-05 00:00:00")
        );
        assert_eq!(
            parse("5-mar-1999", "DD-MON-RR").unwrap(),
            dt("1999-03-05 00:00:00")
        );
        assert_eq!(
            parse("2024/3/5 2:07:09 PM", "YYYY-MM-DD HH:MI:SS AM").unwrap(),
            dt("2024-03-05 14:07:09")
        );
        assert_eq!(
            parse("2024-03-05 14:07:09.5", "YYYY-MM-DD HH24:MI:SS.FF").unwrap(),
            dt("2024-03-05 14:07:09.5")
        );
        assert_eq!(parse("2024-02-30", "YYYY-MM-DD").unwrap_err().code, 1839);
        assert_eq!(parse("2024-13-01", "YYYY-MM-DD").unwrap_err().code, 1843);
        assert_eq!(
            parse("2024-01-01 junk", "YYYY-MM-DD").unwrap_err().code,
            1830
        );
    }

    #[test]
    fn month_arithmetic() {
        assert_eq!(
            add_months(&dt("2024-01-31 10:00:00"), 1),
            dt("2024-02-29 10:00:00")
        );
        assert_eq!(
            add_months(&dt("2024-02-29 00:00:00"), 12),
            dt("2025-02-28 00:00:00")
        );
        assert_eq!(
            add_months(&dt("2024-03-15 00:00:00"), -3),
            dt("2023-12-15 00:00:00")
        );
        assert_eq!(
            trunc(&dt("2024-03-15 10:11:12"), "MM").unwrap(),
            dt("2024-03-01 00:00:00")
        );
    }
}
