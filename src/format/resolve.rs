//! YAML 1.1 implicit typing of plain scalars, as js-yaml 5.4.3's
//! `YAML11_SCHEMA` does it (tags tried in order: null, bool, int, float,
//! timestamp, merge; anything else is a string).

use std::sync::LazyLock;

use regex::Regex;

use super::js;
use super::value::Value;

/// What a plain scalar resolves to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Null,
    Bool,
    Int,
    Float,
    Timestamp,
    Merge,
    Str,
}

pub enum Resolved {
    Value(Value),
    Merge,
}

static INT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-+]?0b[0-1_]+|[-+]?0[0-7_]+|[-+]?0x[0-9a-fA-F_]+|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+|[-+]?(?:0|[1-9][0-9_]*))$")
        .expect("valid regex")
});
static FLOAT: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-+]?(?:(?:[0-9][0-9_]*)?\.[0-9_]*)(?:[eE][-+][0-9]+)?|[-+]?[0-9][0-9_]*(?::[0-5]?[0-9])+\.[0-9_]*|[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$")
        .expect("valid regex")
});
static FLOAT_SPECIAL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?:[-+]?\.(?:inf|Inf|INF)|\.(?:nan|NaN|NAN))$").expect("valid regex")
});
static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]{4})-([0-9]{2})-([0-9]{2})$").expect("valid regex"));
static TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^([0-9]{4})-([0-9][0-9]?)-([0-9][0-9]?)(?:[Tt]|[ \t]+)([0-9][0-9]?):([0-9]{2}):([0-9]{2})(?:\.([0-9]*))?(?:[ \t]*(Z|([-+])([0-9][0-9]?)(?::([0-9]{2}))?))?$")
        .expect("valid regex")
});

const NULLS: [&str; 5] = ["", "~", "null", "Null", "NULL"];
const TRUES: [&str; 11] = [
    "true", "True", "TRUE", "y", "Y", "yes", "Yes", "YES", "on", "On", "ON",
];
const FALSES: [&str; 11] = [
    "false", "False", "FALSE", "n", "N", "no", "No", "NO", "off", "Off", "OFF",
];

/// Resolve a plain (unquoted, untagged) scalar.
pub fn resolve_plain(source: &str) -> Resolved {
    if NULLS.contains(&source) {
        return Resolved::Value(Value::Null);
    }
    if TRUES.contains(&source) {
        return Resolved::Value(Value::Bool(true));
    }
    if FALSES.contains(&source) {
        return Resolved::Value(Value::Bool(false));
    }
    if let Some(n) = resolve_int(source) {
        return Resolved::Value(Value::Number(n));
    }
    if let Some(n) = resolve_float(source) {
        return Resolved::Value(Value::Number(n));
    }
    if let Some(ms) = resolve_timestamp(source) {
        return Resolved::Value(Value::Date(ms));
    }
    if source == "<<" {
        return Resolved::Merge;
    }
    Resolved::Value(Value::String(source.to_string()))
}

/// The tag a string would get if written plain, used to decide whether it
/// needs quoting.
pub fn plain_kind(source: &str) -> Kind {
    match resolve_plain(source) {
        Resolved::Merge => Kind::Merge,
        Resolved::Value(Value::Null) => Kind::Null,
        Resolved::Value(Value::Bool(_)) => Kind::Bool,
        Resolved::Value(Value::Number(_)) => {
            if resolve_int(source).is_some() {
                Kind::Int
            } else {
                Kind::Float
            }
        }
        Resolved::Value(Value::Date(_)) => Kind::Timestamp,
        Resolved::Value(_) => Kind::Str,
    }
}

/// JS `parseInt(digits, radix)` for digits already validated by the pattern.
fn parse_radix(digits: &str, radix: u32) -> f64 {
    digits
        .chars()
        .filter_map(|c| c.to_digit(radix))
        .fold(0.0, |acc, d| acc * f64::from(radix) + f64::from(d))
}

fn resolve_int(source: &str) -> Option<f64> {
    if !INT.is_match(source) {
        return None;
    }
    let cleaned = source.replace('_', "");
    let (sign, value) = match cleaned.as_bytes().first() {
        Some(b'-') => (-1.0, &cleaned[1..]),
        Some(b'+') => (1.0, &cleaned[1..]),
        _ => (1.0, cleaned.as_str()),
    };
    let n = if let Some(bin) = value.strip_prefix("0b") {
        parse_radix(bin, 2)
    } else if let Some(hex) = value.strip_prefix("0x") {
        parse_radix(hex, 16)
    } else if value.contains(':') {
        value.split(':').fold(0.0, |acc, part| {
            acc * 60.0 + part.parse::<f64>().unwrap_or(f64::NAN)
        })
    } else if value != "0" && value.starts_with('0') {
        parse_radix(value, 8)
    } else {
        parse_radix(value, 10)
    };
    let n = sign * n;
    n.is_finite().then_some(n)
}

/// JS `parseFloat` for strings already shaped like `digits.digits(e±digits)`.
fn parse_float_prefix(value: &str) -> f64 {
    // "1." and ".5" parse in Rust; only a bare "." (or "." + exponent) fails,
    // where parseFloat also gives NaN.
    value.parse::<f64>().unwrap_or(f64::NAN)
}

fn resolve_float(source: &str) -> Option<f64> {
    if !FLOAT.is_match(source) {
        return None;
    }
    let lower = source.to_lowercase().replace('_', "");
    let (sign, value) = match lower.as_bytes().first() {
        Some(b'-') => (-1.0, &lower[1..]),
        Some(b'+') => (1.0, &lower[1..]),
        _ => (1.0, lower.as_str()),
    };
    if value == ".inf" {
        return Some(sign * f64::INFINITY);
    }
    if value == ".nan" {
        return Some(f64::NAN);
    }
    let n = if value.contains(':') {
        sign * value.split(':').fold(0.0, |acc, part| {
            acc * 60.0 + part.parse::<f64>().unwrap_or(f64::NAN)
        })
    } else {
        sign * parse_float_prefix(value)
    };
    (n.is_finite() || FLOAT_SPECIAL.is_match(source)).then_some(n)
}

fn num(caps: &regex::Captures<'_>, i: usize) -> u32 {
    caps.get(i)
        .and_then(|m| m.as_str().parse().ok())
        .unwrap_or(0)
}

fn valid_date(year: i64, month: u32, day: u32) -> bool {
    (1..=12).contains(&month) && day >= 1 && day <= js::days_in_month(year, month)
}

fn resolve_timestamp(source: &str) -> Option<i64> {
    if let Some(caps) = DATE.captures(source) {
        let year = i64::from(num(&caps, 1));
        let (month, day) = (num(&caps, 2), num(&caps, 3));
        return valid_date(year, month, day).then(|| js::utc_millis(year, month, day, 0, 0, 0, 0));
    }
    let caps = TIMESTAMP.captures(source)?;
    let year = i64::from(num(&caps, 1));
    let (month, day) = (num(&caps, 2), num(&caps, 3));
    let (hour, minute, second) = (num(&caps, 4), num(&caps, 5), num(&caps, 6));
    if hour > 23 || minute > 59 || second > 59 || !valid_date(year, month, day) {
        return None;
    }
    let fraction = caps.get(7).map_or(0, |m| {
        let mut digits: String = m.as_str().chars().take(3).collect();
        while digits.len() < 3 {
            digits.push('0');
        }
        digits.parse().unwrap_or(0)
    });
    let mut ms = js::utc_millis(year, month, day, hour, minute, second, fraction);
    if let Some(sign) = caps.get(9) {
        let (oh, om) = (num(&caps, 10), num(&caps, 11));
        if oh > 23 || om > 59 {
            return None;
        }
        let offset = i64::from(oh * 60 + om) * 60_000;
        ms -= if sign.as_str() == "-" {
            -offset
        } else {
            offset
        };
    }
    Some(ms)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kind(s: &str) -> Kind {
        plain_kind(s)
    }

    #[test]
    fn yaml11_keywords_should_not_be_strings() {
        for s in [
            "yes",
            "No",
            "on",
            "y",
            "~",
            "null",
            "0123",
            "0x1F",
            "1_000",
            "12:30",
            ".5",
            ".inf",
            "2026-10-19",
        ] {
            assert_ne!(kind(s), Kind::Str, "{s}");
        }
    }

    #[test]
    fn core_only_forms_should_stay_strings() {
        for s in ["0o17", "1e3", "2026-W44", "26-955", "08", "2026-02-30", "."] {
            assert_eq!(kind(s), Kind::Str, "{s}");
        }
    }

    #[test]
    fn octal_and_sexagesimal_should_parse_like_js() {
        assert!(matches!(resolve_plain("0123"), Resolved::Value(Value::Number(n)) if n == 83.0));
        assert!(matches!(resolve_plain("1:30"), Resolved::Value(Value::Number(n)) if n == 90.0));
    }

    #[test]
    fn timestamp_offsets_should_convert_to_utc() {
        let Resolved::Value(Value::Date(ms)) = resolve_plain("2026-01-05t01:02:03+01") else {
            panic!("not a date");
        };
        assert_eq!(js::iso_string(ms), "2026-01-05T00:02:03.000Z");
    }
}
