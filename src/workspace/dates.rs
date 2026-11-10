//! Date rules from the original app. Due dates are `YYYY-MM-DD` strings.
//! Recurrence and "next occurrence" follow the app's arithmetic exactly,
//! including JS `Date` rollover (Jan 31 + 1 month = Mar 3), while task-ID
//! years and "completed on this day" use local time, as the app does.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, TimeZone};
use regex::Regex;

use crate::format::{Value, js};

pub const RECURRENCES: [&str; 4] = ["daily", "weekly", "monthly", "yearly"];

static DATE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-\d{2}-\d{2}$").expect("valid regex"));
static WEEK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\d{4}-W\d{2}$").expect("valid regex"));

pub fn is_date(s: &str) -> bool {
    DATE.is_match(s)
}

pub fn is_week_id(s: &str) -> bool {
    WEEK.is_match(s)
}

pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

/// `new Date().toISOString()`.
pub fn now_iso() -> String {
    js::iso_string(now_millis())
}

/// JS truthiness.
pub fn is_truthy(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => *n != 0.0 && !n.is_nan(),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

/// The app's `toDateString`: a `YYYY-MM-DD` for dates and ISO timestamps
/// (UTC), anything else unchanged.
pub fn to_date_string(v: &Value) -> String {
    match v {
        Value::Date(ms) => js::iso_date(*ms),
        Value::String(s) if is_date(s) => s.clone(),
        Value::String(s) => {
            crate::format::parse_timestamp(s).map_or_else(|| s.clone(), js::iso_date)
        }
        other => other.to_js_string(),
    }
}

/// `due || null`, normalized to a date string.
pub fn due_of(v: Option<&Value>) -> Option<String> {
    v.filter(|v| is_truthy(v)).map(to_date_string)
}

pub fn valid_recurrence(v: Option<&Value>) -> Option<&'static str> {
    let s = v?.as_str()?;
    RECURRENCES.iter().copied().find(|r| *r == s)
}

fn parse_ymd(s: &str) -> Option<(i64, u32, u32)> {
    if !is_date(s) {
        return None;
    }
    Some((
        s[0..4].parse().ok()?,
        s[5..7].parse().ok()?,
        s[8..10].parse().ok()?,
    ))
}

/// Whether a recurring task due on `due` also falls on `target`.
pub fn is_recurring_on_date(due: &str, recurrence: Option<&str>, target: &str) -> bool {
    let Some(recurrence) = recurrence else {
        return false;
    };
    if due.is_empty() || target < due {
        return false;
    }
    if target == due {
        return true;
    }
    let (Some((dy, dm, dd)), Some((ty, tm, td))) = (parse_ymd(due), parse_ymd(target)) else {
        // JS compares NaN dates: only "daily" still matches
        return recurrence == "daily";
    };
    match recurrence {
        "daily" => true,
        "weekly" => (js::days_from_civil(ty, tm, td) - js::days_from_civil(dy, dm, dd)) % 7 == 0,
        "monthly" => dd == td,
        "yearly" => dm == tm && dd == td,
        _ => false,
    }
}

/// The next due date after completing a recurring task, with JS overflow
/// semantics (`setUTCMonth(+1)` on Jan 31 lands on Mar 3).
pub fn next_occurrence(due: &str, recurrence: &str) -> Option<String> {
    let (y, m, d) = parse_ymd(due)?;
    let days = match recurrence {
        "daily" => js::days_from_civil(y, m, d) + 1,
        "weekly" => js::days_from_civil(y, m, d) + 7,
        // days_from_civil is linear in the day, so day overflow rolls into the next month
        "monthly" if m == 12 => js::days_from_civil(y + 1, 1, d),
        "monthly" => js::days_from_civil(y, m + 1, d),
        "yearly" => js::days_from_civil(y + 1, m, d),
        _ => return None,
    };
    Some(js::iso_date(days * 86_400_000))
}

/// Two-digit local year, the task ID prefix (`26` for 2026).
pub fn id_year_prefix() -> String {
    format!("{:02}", Local::now().year().rem_euclid(100))
}

/// A timestamp as milliseconds since the epoch.
pub fn millis_of(v: &Value) -> Option<i64> {
    match v {
        Value::Date(ms) => Some(*ms),
        Value::String(s) => crate::format::parse_timestamp(s),
        _ => None,
    }
}

/// The local calendar date of a timestamp.
pub fn local_date_of(v: &Value) -> Option<String> {
    Local
        .timestamp_millis_opt(millis_of(v)?)
        .single()
        .map(|dt| dt.format("%Y-%m-%d").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monthly_should_roll_over_like_js() {
        assert_eq!(
            next_occurrence("2026-01-31", "monthly").as_deref(),
            Some("2026-03-03")
        );
    }

    #[test]
    fn yearly_on_leap_day_should_roll_over_like_js() {
        assert_eq!(
            next_occurrence("2028-02-29", "yearly").as_deref(),
            Some("2029-03-01")
        );
    }

    #[test]
    fn december_monthly_should_wrap_the_year() {
        assert_eq!(
            next_occurrence("2026-12-15", "monthly").as_deref(),
            Some("2027-01-15")
        );
    }

    #[test]
    fn weekly_recurrence_should_match_every_seventh_day() {
        assert!(is_recurring_on_date(
            "2026-11-02",
            Some("weekly"),
            "2026-11-16"
        ));
        assert!(!is_recurring_on_date(
            "2026-11-02",
            Some("weekly"),
            "2026-11-15"
        ));
    }

    #[test]
    fn dates_before_due_should_not_recur() {
        assert!(!is_recurring_on_date(
            "2026-11-02",
            Some("daily"),
            "2026-11-01"
        ));
    }

    #[test]
    fn timestamps_should_become_utc_dates() {
        assert_eq!(
            to_date_string(&Value::String("2025-11-03T00:00:00.000Z".into())),
            "2025-11-03"
        );
    }
}
