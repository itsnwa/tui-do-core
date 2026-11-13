//! Date rules from the original app. Due dates are `YYYY-MM-DD` strings.
//! Recurrence and "next occurrence" follow the app's arithmetic exactly,
//! including JS `Date` rollover (Jan 31 + 1 month = Mar 3), while task-ID
//! years and "completed on this day" use local time, as the app does.

use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{Datelike, Local, TimeZone};
use regex::Regex;

use crate::format::{Value, js};

/// The recurrences that are one word; `days:` and `hours:` take a value.
pub const RECURRENCES: [&str; 5] = ["daily", "weekly", "monthly", "yearly", "weekdays"];

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// How a recurring task repeats, from its `recurrence`: one of
/// [`RECURRENCES`], `days:mon,wed,fri` or `hours:4`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recurrence {
    Daily,
    Weekly,
    Monthly,
    Yearly,
    /// Monday to Friday.
    Weekdays,
    /// Those days of the week: bit 0 is Monday.
    Days(u8),
    /// Every so many hours (1 to 24), from the task's `at`.
    Hours(u32),
}

impl Recurrence {
    pub fn parse(s: &str) -> Option<Self> {
        if let Some(days) = s.strip_prefix("days:") {
            let mut mask = 0u8;
            for day in days.split(',') {
                mask |= 1 << WEEKDAYS.iter().position(|d| *d == day.trim())?;
            }
            return (mask != 0).then_some(Self::Days(mask));
        }
        if let Some(hours) = s.strip_prefix("hours:") {
            let hours = hours.trim().parse().ok().filter(|h| (1..=24).contains(h))?;
            return Some(Self::Hours(hours));
        }
        Some(match s {
            "daily" => Self::Daily,
            "weekly" => Self::Weekly,
            "monthly" => Self::Monthly,
            "yearly" => Self::Yearly,
            "weekdays" => Self::Weekdays,
            _ => return None,
        })
    }

    /// Whether it can start on `date`: any date, except for the ones that go
    /// by the day of the week.
    pub fn falls_on(self, date: &str) -> bool {
        let Some((y, m, d)) = parse_ymd(date) else {
            return false;
        };
        self.on_weekday(weekday_of(js::days_from_civil(y, m, d)))
            .unwrap_or(true)
    }

    /// Whether it repeats on `weekday` (0 is Monday), for the ones that go by
    /// the day of the week.
    fn on_weekday(self, weekday: u32) -> Option<bool> {
        match self {
            Self::Weekdays => Some(weekday < 5),
            Self::Days(mask) => Some(mask & (1 << weekday) != 0),
            _ => None,
        }
    }
}

impl std::fmt::Display for Recurrence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Daily => f.write_str("daily"),
            Self::Weekly => f.write_str("weekly"),
            Self::Monthly => f.write_str("monthly"),
            Self::Yearly => f.write_str("yearly"),
            Self::Weekdays => f.write_str("weekdays"),
            Self::Days(mask) => {
                let days: Vec<_> = (0..7)
                    .filter(|d| mask & (1 << d) != 0)
                    .map(|d| WEEKDAYS[d])
                    .collect();
                write!(f, "days:{}", days.join(","))
            }
            Self::Hours(hours) => write!(f, "hours:{hours}"),
        }
    }
}

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

/// A task's `recurrence`, written the one way it's stored.
pub fn valid_recurrence(v: Option<&Value>) -> Option<String> {
    Recurrence::parse(v?.as_str()?).map(|r| r.to_string())
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

/// The day of the week of a day since the epoch, 0 being Monday.
fn weekday_of(days: i64) -> u32 {
    // 1970-01-01 was a Thursday
    (days + 3).rem_euclid(7) as u32
}

/// Whether a recurring task due on `due` also falls on `target`.
pub fn is_recurring_on_date(due: &str, recurrence: Option<&str>, target: &str) -> bool {
    let Some(recurrence) = recurrence.and_then(Recurrence::parse) else {
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
        return matches!(recurrence, Recurrence::Daily | Recurrence::Hours(_));
    };
    let days = js::days_from_civil(ty, tm, td);
    match recurrence {
        // Every day has a slot when they're at most 24 hours apart
        Recurrence::Daily | Recurrence::Hours(_) => true,
        Recurrence::Weekly => (days - js::days_from_civil(dy, dm, dd)) % 7 == 0,
        Recurrence::Monthly => dd == td,
        Recurrence::Yearly => dm == tm && dd == td,
        Recurrence::Weekdays | Recurrence::Days(_) => {
            recurrence.on_weekday(weekday_of(days)).unwrap_or(false)
        }
    }
}

/// The next due date after `due`, with JS overflow semantics
/// (`setUTCMonth(+1)` on Jan 31 lands on Mar 3). None for `hours:`, which
/// moves by the time of day instead (see `schedule`).
pub fn next_occurrence(due: &str, recurrence: &str) -> Option<String> {
    next_date(due, Recurrence::parse(recurrence)?)
}

/// [`next_occurrence`], for a recurrence that's been parsed.
pub fn next_date(due: &str, recurrence: Recurrence) -> Option<String> {
    let (y, m, d) = parse_ymd(due)?;
    let today = js::days_from_civil(y, m, d);
    let days = match recurrence {
        Recurrence::Daily => today + 1,
        Recurrence::Weekly => today + 7,
        // days_from_civil is linear in the day, so day overflow rolls into the next month
        Recurrence::Monthly if m == 12 => js::days_from_civil(y + 1, 1, d),
        Recurrence::Monthly => js::days_from_civil(y, m + 1, d),
        Recurrence::Yearly => js::days_from_civil(y + 1, m, d),
        Recurrence::Weekdays | Recurrence::Days(_) => (today + 1..=today + 7)
            .find(|days| recurrence.on_weekday(weekday_of(*days)) == Some(true))?,
        Recurrence::Hours(_) => return None,
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
    fn recurrences_should_be_stored_one_way() {
        let stored = |s: &str| Recurrence::parse(s).map(|r| r.to_string());
        assert_eq!(stored("weekdays").as_deref(), Some("weekdays"));
        assert_eq!(
            stored("days:fri, mon,wed").as_deref(),
            Some("days:mon,wed,fri")
        );
        assert_eq!(stored("hours:4").as_deref(), Some("hours:4"));
        assert_eq!(stored("days:"), None);
        assert_eq!(stored("days:mon,funday"), None);
        assert_eq!(stored("hours:0"), None);
        assert_eq!(stored("hours:25"), None);
        assert_eq!(stored("fortnightly"), None);
    }

    #[test]
    fn weekdays_should_skip_the_weekend() {
        // 2026-11-13 is a Friday
        assert_eq!(
            next_occurrence("2026-11-13", "weekdays").as_deref(),
            Some("2026-11-16")
        );
        assert_eq!(
            next_occurrence("2026-11-10", "weekdays").as_deref(),
            Some("2026-11-11")
        );
        assert!(!is_recurring_on_date(
            "2026-11-13",
            Some("weekdays"),
            "2026-11-14"
        ));
        assert!(is_recurring_on_date(
            "2026-11-13",
            Some("weekdays"),
            "2026-11-16"
        ));
    }

    #[test]
    fn days_should_move_to_the_next_day_listed() {
        let mwf = "days:mon,wed,fri";
        assert_eq!(
            next_occurrence("2026-11-13", mwf).as_deref(),
            Some("2026-11-16")
        );
        assert_eq!(
            next_occurrence("2026-11-16", mwf).as_deref(),
            Some("2026-11-18")
        );
        assert_eq!(
            next_occurrence("2026-11-16", "days:mon").as_deref(),
            Some("2026-11-23")
        );
        assert!(is_recurring_on_date("2026-11-13", Some(mwf), "2026-11-18"));
        assert!(!is_recurring_on_date("2026-11-13", Some(mwf), "2026-11-17"));
    }

    #[test]
    fn hours_should_fall_on_every_day_but_not_move_the_date() {
        assert!(is_recurring_on_date(
            "2026-11-13",
            Some("hours:6"),
            "2026-11-20"
        ));
        assert_eq!(next_occurrence("2026-11-13", "hours:6"), None);
    }

    #[test]
    fn timestamps_should_become_utc_dates() {
        assert_eq!(
            to_date_string(&Value::String("2025-11-03T00:00:00.000Z".into())),
            "2025-11-03"
        );
    }
}
