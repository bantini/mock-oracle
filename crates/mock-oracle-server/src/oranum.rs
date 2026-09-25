//! Oracle's NUMBER wire format: an exponent byte followed by base-100 digits.
//! Ported from the encoder and decoder in python-oracledb / node-oracledb thin.

use mock_oracle_core::Value;

const MAX_DIGITS: usize = 40;

/// Encodes a decimal string (e.g. `-12.5`) without its length prefix.
pub fn encode(value: &str) -> Option<Vec<u8>> {
    let (negative, mut digits) = match value.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, value.to_string()),
    };
    let mut exponent: i32 = 0;
    if let Some(pos) = digits.find(['e', 'E']) {
        exponent = digits[pos + 1..].parse().ok()?;
        digits.truncate(pos);
    }
    if let Some(pos) = digits.find('.') {
        exponent -= (digits.len() - pos - 1) as i32;
        digits.remove(pos);
    }
    let trimmed = digits.trim_start_matches('0');
    digits = trimmed.to_string();
    let without_trailing = digits.trim_end_matches('0').to_string();
    exponent += (digits.len() - without_trailing.len()) as i32;
    digits = without_trailing;

    if digits.len() > MAX_DIGITS || exponent >= 126 || exponent <= -131 {
        return None;
    }
    if digits.is_empty() {
        return Some(vec![0x80]);
    }
    if exponent % 2 != 0 {
        exponent -= 1;
        digits.push('0');
    }
    if digits.len() % 2 == 1 {
        digits.insert(0, '0');
    }
    let pairs: Vec<u8> = digits
        .as_bytes()
        .chunks(2)
        .map(|p| (p[0] - b'0') * 10 + (p[1] - b'0'))
        .collect();
    let exp_on_wire = ((exponent + digits.len() as i32) / 2 + 192) as u8;
    let mut out = Vec::with_capacity(pairs.len() + 2);
    if negative {
        out.push(!exp_on_wire);
        out.extend(pairs.iter().map(|d| 101 - d));
        if digits.len() < MAX_DIGITS {
            out.push(102);
        }
    } else {
        out.push(exp_on_wire);
        out.extend(pairs.iter().map(|d| d + 1));
    }
    Some(out)
}

pub fn encode_value(v: &Value) -> Option<Vec<u8>> {
    match v {
        Value::Number(n) => encode(&n.normalized().to_scientific_notation()),
        _ => None,
    }
}

/// Decodes NUMBER bytes (without the length prefix) to a decimal string.
pub fn decode(buf: &[u8]) -> String {
    let Some(&first) = buf.first() else {
        return "0".into();
    };
    let positive = first & 0x80 != 0;
    let exponent = if positive { first } else { !first } as i32 - 193;
    let mut decimal_point = exponent * 2 + 2;
    if buf.len() == 1 {
        return if positive {
            "0".into()
        } else {
            "-1e126".into()
        };
    }
    let mut n = buf.len();
    if !positive && buf[n - 1] == 102 {
        n -= 1;
    }
    let mut digits = String::new();
    for (i, &b) in buf[1..n].iter().enumerate() {
        let d = if positive {
            b as i32 - 1
        } else {
            101 - b as i32
        };
        let hi = d / 10;
        let lo = d % 10;
        if hi == 0 && i == 0 {
            decimal_point -= 1;
        } else {
            digits.push(char::from(b'0' + hi as u8));
        }
        digits.push(char::from(b'0' + lo as u8));
    }
    let s = format!(
        "{}0.{}e{}",
        if positive { "" } else { "-" },
        digits,
        decimal_point
    );
    Value::parse_number(&s)
        .map(|v| v.to_string())
        .unwrap_or_else(|| "0".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_encodings() {
        // Values checked against node-oracledb's encoder.
        assert_eq!(encode("0").unwrap(), vec![0x80]);
        assert_eq!(encode("1").unwrap(), vec![0xc1, 0x02]);
        assert_eq!(encode("100").unwrap(), vec![0xc2, 0x02]);
        assert_eq!(encode("-1").unwrap(), vec![0x3e, 0x64, 0x66]);
        assert_eq!(encode("0.5").unwrap(), vec![0xc0, 0x33]);
        assert_eq!(encode("1E+2").unwrap(), vec![0xc2, 0x02]);
    }

    #[test]
    fn round_trips() {
        for s in [
            "1",
            "-1",
            "12.5",
            "-0.001",
            "123456789012345678901234567890",
            ".25",
            "1000",
        ] {
            let expected = Value::parse_number(s).unwrap().to_string();
            assert_eq!(decode(&encode(s).unwrap()), expected, "{s}");
        }
    }
}
