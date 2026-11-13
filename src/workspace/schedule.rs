//! When a task's occurrences fall: on its `due` date at `at` (a time of
//! day, `HH:MM`; midnight without one) in `tz` (an IANA time zone; this
//! machine's own without one), repeated by its `recurrence`. The home core
//! starts the agent of a task with `autostart` at each one (see
//! `api::scheduler`), and completing a recurring task moves it to the next
//! one still to come.

use chrono::{DateTime, Duration, Local, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::dates::{self, Recurrence};
use crate::format::{Map, Value};

/// How many occurrences are walked through at most, so a long-forgotten
/// hourly task can't keep anyone busy.
const MAX_STEPS: usize = 100_000;

/// One occurrence: a due date and, for a task with a time, a time of day.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Slot {
    pub due: String,
    pub at: Option<String>,
}

impl Slot {
    /// How it's written in `lastRun` and in a run's heading: `2026-11-14 09:00`.
    pub fn key(&self) -> String {
        match &self.at {
            Some(at) => format!("{} {at}", self.due),
            None => self.due.clone(),
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        let (due, at) = match key.split_once(' ') {
            Some((due, at)) => (due, Some(at)),
            None => (key, None),
        };
        let valid = dates::is_date(due) && at.is_none_or(|at| parse_at(at).is_some());
        valid.then(|| Self {
            due: due.to_string(),
            at: at.map(str::to_string),
        })
    }
}

/// `HH:MM` as hours and minutes.
pub fn parse_at(at: &str) -> Option<(u32, u32)> {
    let (h, m) = at.split_once(':')?;
    if h.len() != 2 || m.len() != 2 {
        return None;
    }
    let (h, m) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some((h, m))
}

pub fn valid_tz(tz: &str) -> bool {
    tz.parse::<Tz>().is_ok()
}

/// The time zone a task's times are in.
#[derive(Clone, Copy, Debug)]
pub enum Zone {
    Named(Tz),
    /// This machine's.
    Local,
}

impl Zone {
    pub fn of(tz: Option<&str>) -> Self {
        tz.and_then(|tz| tz.parse().ok())
            .map_or(Self::Local, Self::Named)
    }

    /// The instant a slot falls on. A time a clock change skips is taken an
    /// hour later; one it goes through twice, the first time.
    pub fn instant(self, slot: &Slot) -> Option<DateTime<Utc>> {
        let date = NaiveDate::parse_from_str(&slot.due, "%Y-%m-%d").ok()?;
        let (h, m) = slot.at.as_deref().map_or(Some((0, 0)), parse_at)?;
        let local = date.and_hms_opt(h, m, 0)?;
        match self {
            Self::Named(tz) => resolve(&tz, local),
            Self::Local => resolve(&Local, local),
        }
    }

    /// The slot an instant falls on.
    pub fn slot(self, t: DateTime<Utc>) -> Slot {
        let local = match self {
            Self::Named(tz) => t.with_timezone(&tz).naive_local(),
            Self::Local => t.with_timezone(&Local).naive_local(),
        };
        Slot {
            due: local.format("%Y-%m-%d").to_string(),
            at: Some(local.format("%H:%M").to_string()),
        }
    }

    /// The occurrence after `slot`.
    fn step(self, slot: &Slot, recurrence: Recurrence) -> Option<Slot> {
        match recurrence {
            Recurrence::Hours(hours) => {
                let next = self.instant(slot)? + Duration::hours(hours.into());
                Some(self.slot(next))
            }
            _ => Some(Slot {
                due: dates::next_date(&slot.due, recurrence)?,
                at: slot.at.clone(),
            }),
        }
    }

    /// Where a recurring task goes when it's completed at `now`: the first
    /// occurrence after `slot` that's still to come.
    pub fn advance(self, slot: &Slot, recurrence: Recurrence, now: DateTime<Utc>) -> Option<Slot> {
        let mut next = self.step(slot, recurrence)?;
        for _ in 0..MAX_STEPS {
            if self.instant(&next)? > now {
                break;
            }
            next = self.step(&next, recurrence)?;
        }
        Some(next)
    }

    /// Where a recurring task goes when it's completed at `now`. One whose
    /// agent starts by itself (`scheduled`) goes to its first occurrence
    /// still to come, which can be the one it's at: a run started by hand
    /// before it is an extra one, and the scheduled runs keep their times.
    /// Any other is done with the occurrence it's at, also when that's
    /// early, and goes on from the next.
    pub fn completed(
        self,
        slot: &Slot,
        recurrence: Recurrence,
        scheduled: bool,
        now: DateTime<Utc>,
    ) -> Option<Slot> {
        if scheduled {
            self.upcoming(slot, Some(recurrence), now)
                .or_else(|| self.advance(slot, recurrence, now))
        } else {
            self.advance(slot, recurrence, now)
        }
    }

    /// The occurrences from `slot` on that have come by `now`, leaving out
    /// the ones up to `after`: the latest, and how many there were.
    pub fn came(
        self,
        slot: &Slot,
        recurrence: Option<Recurrence>,
        after: Option<DateTime<Utc>>,
        now: DateTime<Utc>,
    ) -> Option<(Slot, u32)> {
        let mut latest = None;
        let mut current = slot.clone();
        for _ in 0..MAX_STEPS {
            let t = self.instant(&current)?;
            if t > now {
                break;
            }
            if after.is_none_or(|after| t > after) {
                latest = Some((current.clone(), latest.map_or(1, |(_, n)| n + 1)));
            }
            match recurrence.and_then(|r| self.step(&current, r)) {
                Some(next) => current = next,
                None => break,
            }
        }
        latest
    }

    /// The first occurrence from `slot` on that's still to come.
    pub fn upcoming(
        self,
        slot: &Slot,
        recurrence: Option<Recurrence>,
        now: DateTime<Utc>,
    ) -> Option<Slot> {
        let mut current = slot.clone();
        for _ in 0..MAX_STEPS {
            if self.instant(&current)? > now {
                return Some(current);
            }
            current = self.step(&current, recurrence?)?;
        }
        None
    }
}

fn resolve<Z: TimeZone>(zone: &Z, local: NaiveDateTime) -> Option<DateTime<Utc>> {
    let first = |t: NaiveDateTime| zone.from_local_datetime(&t).earliest();
    first(local)
        .or_else(|| first(local + Duration::hours(1)))
        .map(|t| t.with_timezone(&Utc))
}

/// Starting a task's agent when it comes due: the choices
/// `POST /v0/tasks/{ref}/agent` takes, and whether the agent completes the
/// task itself.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Autostart {
    /// `claude` (the default) or `codex`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub yolo: bool,
    /// The fleet machine to run on; none is the code machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    /// The agent completes the task when it's done, instead of asking you first.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub complete_on_its_own: bool,
}

impl Autostart {
    /// As a task's frontmatter holds it.
    pub fn from_value(value: &Value) -> Option<Self> {
        let Value::Map(map) = value else {
            return None;
        };
        let text = |key: &str| {
            let text = map.get(key).and_then(Value::as_str);
            text.filter(|t| !t.is_empty()).map(str::to_string)
        };
        let flag = |key: &str| matches!(map.get(key), Some(Value::Bool(true)));
        Some(Self {
            agent: text("agent"),
            model: text("model"),
            effort: text("effort"),
            permission_mode: text("permissionMode"),
            yolo: flag("yolo"),
            device: text("device"),
            complete_on_its_own: flag("completeOnItsOwn"),
        })
    }

    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        let texts = [
            ("agent", &self.agent),
            ("model", &self.model),
            ("effort", &self.effort),
            ("permissionMode", &self.permission_mode),
            ("device", &self.device),
        ];
        for (key, text) in texts {
            if let Some(text) = text.as_deref().filter(|t| !t.is_empty()) {
                map.insert(key.into(), text.into());
            }
        }
        for (key, flag) in [
            ("yolo", self.yolo),
            ("completeOnItsOwn", self.complete_on_its_own),
        ] {
            if flag {
                map.insert(key.into(), true.into());
            }
        }
        Value::Map(map)
    }

    pub fn validate(&self) -> Result<(), String> {
        match self.agent.as_deref() {
            None | Some("claude" | "codex") => Ok(()),
            Some(other) => Err(format!(
                "autostart.agent must be claude or codex, got {other:?}"
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(due: &str, at: Option<&str>) -> Slot {
        Slot {
            due: due.into(),
            at: at.map(Into::into),
        }
    }

    fn oslo() -> Zone {
        Zone::of(Some("Europe/Oslo"))
    }

    /// An instant given as Oslo's local time.
    fn at(local: &str) -> DateTime<Utc> {
        let (due, at) = local.split_once(' ').expect("a date and a time");
        oslo().instant(&slot(due, Some(at))).expect("an instant")
    }

    #[test]
    fn times_should_be_read_in_the_task_s_zone() {
        let t = oslo().instant(&slot("2026-11-13", Some("09:00")));
        assert_eq!(
            t.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-11-13T08:00:00+00:00")
        );
        let summer = oslo().instant(&slot("2026-07-01", Some("09:00")));
        assert_eq!(
            summer.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-07-01T07:00:00+00:00")
        );
    }

    #[test]
    fn a_time_the_clock_skips_should_be_taken_an_hour_later() {
        // Oslo's clocks went from 02:00 to 03:00 on 29 March 2026
        let t = oslo().instant(&slot("2026-03-29", Some("02:30")));
        assert_eq!(
            t.map(|t| t.to_rfc3339()).as_deref(),
            Some("2026-03-29T01:30:00+00:00")
        );
    }

    #[test]
    fn keys_should_read_back() {
        let timed = slot("2026-11-14", Some("09:00"));
        assert_eq!(timed.key(), "2026-11-14 09:00");
        assert_eq!(Slot::from_key("2026-11-14 09:00"), Some(timed));
        assert_eq!(Slot::from_key("2026-11-14"), Some(slot("2026-11-14", None)));
        assert_eq!(Slot::from_key("2026-11-14 9am"), None);
    }

    #[test]
    fn times_of_day_should_be_two_digit_hours_and_minutes() {
        assert_eq!(parse_at("09:05"), Some((9, 5)));
        assert_eq!(parse_at("23:59"), Some((23, 59)));
        assert_eq!(parse_at("24:00"), None);
        assert_eq!(parse_at("9:00"), None);
    }

    #[test]
    fn completing_on_time_should_move_one_occurrence_on() {
        let next = oslo().advance(
            &slot("2026-11-13", Some("09:00")),
            Recurrence::Daily,
            at("2026-11-13 09:30"),
        );
        assert_eq!(next, Some(slot("2026-11-14", Some("09:00"))));
    }

    #[test]
    fn completing_early_should_still_move_on() {
        let next = oslo().advance(
            &slot("2026-11-20", None),
            Recurrence::Weekly,
            at("2026-11-18 12:00"),
        );
        assert_eq!(next, Some(slot("2026-11-27", None)));
    }

    #[test]
    fn completing_late_should_skip_the_occurrences_that_passed() {
        let daily = oslo().advance(
            &slot("2026-11-10", None),
            Recurrence::Daily,
            at("2026-11-13 12:00"),
        );
        assert_eq!(daily, Some(slot("2026-11-14", None)));
        // Today's 09:00 is still to come at 08:00
        let timed = oslo().advance(
            &slot("2026-11-10", Some("09:00")),
            Recurrence::Weekdays,
            at("2026-11-13 08:00"),
        );
        assert_eq!(timed, Some(slot("2026-11-13", Some("09:00"))));
    }

    #[test]
    fn a_run_by_hand_should_leave_the_next_scheduled_one_where_it_is() {
        let tomorrow = slot("2026-11-14", Some("09:00"));
        let by_hand = oslo().completed(&tomorrow, Recurrence::Daily, true, at("2026-11-13 11:30"));
        assert_eq!(by_hand, Some(tomorrow.clone()));
        // The scheduled run itself moves on
        let today = slot("2026-11-13", Some("09:00"));
        let scheduled = oslo().completed(&today, Recurrence::Daily, true, at("2026-11-13 09:30"));
        assert_eq!(scheduled, Some(tomorrow.clone()));
        // A task without a schedule of its own is done with the occurrence it's at
        let early = oslo().completed(&tomorrow, Recurrence::Daily, false, at("2026-11-13 11:30"));
        assert_eq!(early, Some(slot("2026-11-15", Some("09:00"))));
    }

    #[test]
    fn hours_should_move_the_time_and_the_date() {
        let next = oslo().advance(
            &slot("2026-11-13", Some("21:00")),
            Recurrence::Hours(4),
            at("2026-11-13 21:10"),
        );
        assert_eq!(next, Some(slot("2026-11-14", Some("01:00"))));
    }

    #[test]
    fn came_should_count_the_occurrences_after_the_last_run() {
        let start = slot("2026-11-10", Some("09:00"));
        let now = at("2026-11-13 10:00");
        let all = oslo().came(&start, Some(Recurrence::Daily), None, now);
        assert_eq!(all, Some((slot("2026-11-13", Some("09:00")), 4)));
        let since = oslo().came(
            &start,
            Some(Recurrence::Daily),
            Some(at("2026-11-11 09:00")),
            now,
        );
        assert_eq!(since, Some((slot("2026-11-13", Some("09:00")), 2)));
        let none = oslo().came(
            &start,
            Some(Recurrence::Daily),
            Some(at("2026-11-13 09:00")),
            now,
        );
        assert_eq!(none, None);
    }

    #[test]
    fn a_task_that_doesn_t_repeat_should_come_once() {
        let once = slot("2026-11-13", Some("09:00"));
        assert_eq!(oslo().came(&once, None, None, at("2026-11-13 08:59")), None);
        assert_eq!(
            oslo().came(&once, None, None, at("2026-11-20 08:00")),
            Some((once.clone(), 1))
        );
        assert_eq!(oslo().upcoming(&once, None, at("2026-11-20 08:00")), None);
    }

    #[test]
    fn upcoming_should_be_the_next_occurrence_still_to_come() {
        let start = slot("2026-11-12", Some("09:00"));
        assert_eq!(
            oslo().upcoming(&start, Some(Recurrence::Weekdays), at("2026-11-13 10:00")),
            Some(slot("2026-11-16", Some("09:00")))
        );
    }

    #[test]
    fn autostart_should_round_trip_through_the_frontmatter() {
        let autostart = Autostart {
            agent: Some("claude".into()),
            permission_mode: Some("auto".into()),
            complete_on_its_own: true,
            ..Autostart::default()
        };
        assert_eq!(
            Autostart::from_value(&autostart.to_value()),
            Some(autostart)
        );
        assert_eq!(Autostart::from_value(&Value::Null), None);
    }
}
