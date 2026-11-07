//! JavaScript semantics the workspace format depends on.
//!
//! The workspace files were written by a JS app (js-yaml 5.4.3), so a few JS
//! behaviors leak into the format: string lengths count UTF-16 code units,
//! `String.prototype.trim` strips a different set of characters than Rust's
//! `trim`, dates are millisecond timestamps printed with `toISOString`, and
//! numbers are printed the way `Number.prototype.toString` prints them.

/// `str.length` in JS: the number of UTF-16 code units.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// Characters matched by JS `\s` (and stripped by `String.prototype.trim`).
pub fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{0B}' | '\u{0C}' | '\r' | ' ' | '\u{A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

const MS_PER_DAY: i64 = 86_400_000;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

pub fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// Milliseconds since the epoch for a UTC date and time.
pub fn utc_millis(year: i64, month: u32, day: u32, h: u32, m: u32, s: u32, ms: u32) -> i64 {
    days_from_civil(year, month, day) * MS_PER_DAY
        + i64::from(h) * 3_600_000
        + i64::from(m) * 60_000
        + i64::from(s) * 1000
        + i64::from(ms)
}

/// `Date.prototype.toISOString`.
pub fn iso_string(millis: i64) -> String {
    let days = millis.div_euclid(MS_PER_DAY);
    let rem = millis.rem_euclid(MS_PER_DAY);
    let (year, month, day) = civil_from_days(days);
    let (h, m, s, ms) = (
        rem / 3_600_000,
        rem / 60_000 % 60,
        rem / 1000 % 60,
        rem % 1000,
    );
    let year = if (0..=9999).contains(&year) {
        format!("{year:04}")
    } else {
        format!("{}{:06}", if year < 0 { '-' } else { '+' }, year.abs())
    };
    format!("{year}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{ms:03}Z")
}

/// `YYYY-MM-DD` of a UTC timestamp.
pub fn iso_date(millis: i64) -> String {
    iso_string(millis)[..10].to_string()
}

/// `Number.prototype.toString()` (radix 10).
pub fn number_to_string(n: f64) -> String {
    if n.is_nan() {
        return "NaN".into();
    }
    if n.is_infinite() {
        return if n > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if n == 0.0 {
        return "0".into();
    }
    // Shortest round-trip digits and decimal exponent, then lay them out the
    // way ECMAScript's Number::toString does.
    let sci = format!("{:e}", n.abs());
    let (mantissa, exp) = sci.split_once('e').unwrap_or((&sci, "0"));
    let digits: String = mantissa.chars().filter(char::is_ascii_digit).collect();
    let exp: i32 = exp.parse().unwrap_or(0);
    let k = digits.len() as i32;
    let point = exp + 1; // position of the decimal point relative to the digits
    let body = if k <= point && point <= 21 {
        format!("{digits}{}", "0".repeat((point - k) as usize))
    } else if 0 < point && point <= 21 {
        let (int, frac) = digits.split_at(point as usize);
        format!("{int}.{frac}")
    } else if -6 < point && point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else {
        let e = point - 1;
        let sign = if e < 0 { '-' } else { '+' };
        let (first, rest) = digits.split_at(1);
        if rest.is_empty() {
            format!("{first}e{sign}{}", e.abs())
        } else {
            format!("{first}.{rest}e{sign}{}", e.abs())
        }
    };
    if n < 0.0 { format!("-{body}") } else { body }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_should_match_js() {
        let cases = [
            (1.5, "1.5"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (1e-7, "1e-7"),
            (0.000001, "0.000001"),
            (-0.1, "-0.1"),
            (123456789012345680000.0, "123456789012345680000"),
        ];
        for (n, expected) in cases {
            assert_eq!(number_to_string(n), expected, "{n}");
        }
    }

    #[test]
    fn iso_string_should_round_trip_dates() {
        let ms = utc_millis(2026, 3, 1, 0, 0, 0, 0);
        assert_eq!(iso_string(ms), "2026-03-01T00:00:00.000Z");
    }

    #[test]
    fn iso_string_should_handle_pre_epoch_dates() {
        let ms = utc_millis(1969, 12, 31, 23, 59, 59, 999);
        assert_eq!(iso_string(ms), "1969-12-31T23:59:59.999Z");
    }

    #[test]
    fn trim_should_strip_bom_but_not_next_line() {
        assert_eq!(trim("\u{FEFF} a \u{85}"), "a \u{85}");
    }
}
