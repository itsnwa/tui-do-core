//! Starting the agents of tasks with `autostart` as they come due (see
//! `workspace::schedule`), on the home core. Each occurrence is handled
//! once, and recorded as the task's `lastRun`: its agent starts, or, while
//! the previous run is still open, it's counted in `skipped`. Occurrences
//! that passed while the home or the task's machine was down come to one
//! run, as soon as it can start.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use tokio::sync::broadcast::error::RecvError;

use super::AppState;
use super::agent::{self, ScheduledRun, StartAgent};
use crate::activity::Activity;
use crate::session::Event;
use crate::workspace::schedule::Slot;
use crate::workspace::tasks::{Task, TaskFilter};

/// How long the home waits after it starts before it starts any runs, so
/// it has heard from the fleet's machines which sessions they have.
const SETTLE: Duration = Duration::from_secs(20);

/// How long a run that couldn't start waits before it's tried again,
/// unless its machine comes back first.
const RETRY: Duration = Duration::from_secs(5 * 60);

/// How long the scheduler sleeps at most. A Mac that was asleep wakes it no
/// later than this after the time it should have.
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);

/// What to do about a task with `autostart`.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Nothing new has come; look again at `next`.
    Wait { next: Option<DateTime<Utc>> },
    /// `run` came while the previous run is still open. It's skipped, with
    /// the `missed` occurrences since the last run (it included).
    Skip { run: Slot, missed: u32 },
    /// Start a run for `run`; `missed` others came and went before it. An
    /// idle session from an earlier run is closed first, on the machine in
    /// `close`.
    Start {
        run: Slot,
        missed: u32,
        close: Option<String>,
    },
}

/// The rule for one task, given what its activity is now.
fn decide(task: &Task, activity: Option<&Activity>, now: DateTime<Utc>) -> Action {
    let zone = task.zone();
    let Some(slot) = task.slot() else {
        return Action::Wait { next: None };
    };
    let recurrence = task.repeats();
    let last_run = task.last_run.as_deref().and_then(Slot::from_key);
    let after = last_run.and_then(|run| zone.instant(&run));
    let Some((run, count)) = zone.came(&slot, recurrence, after, now) else {
        let next = zone.upcoming(&slot, recurrence, now);
        return Action::Wait {
            next: next.and_then(|next| zone.instant(&next)),
        };
    };
    // Its agent is at work or waiting for an answer, even if nobody started the task
    let busy = matches!(activity.map(|a| a.state), Some("working" | "needs-you"));
    if task.state == "in-progress" || busy {
        return Action::Skip { run, missed: count };
    }
    Action::Start {
        run,
        missed: count - 1,
        close: activity.and_then(|a| a.device.clone()),
    }
}

/// How starting a run went.
enum Started {
    Yes,
    /// The task's session turned out to be running.
    Busy,
    Failed(String),
}

/// Close a task's session on a machine, as `DELETE /v0/sessions/{id}` does.
async fn close_session(state: &AppState, device: &str, slug: &str) -> Result<(), String> {
    if device == state.config.get().device_id {
        if state.registry.remove(slug) {
            crate::uploads::remove(&state.core_dir.join("uploads"), slug);
        }
        return Ok(());
    }
    let machine = state
        .fleet
        .device(device)
        .ok_or_else(|| format!("{device} isn't in the fleet"))?;
    let res = state
        .fleet
        .client()
        .delete(format!("{}/v0/sessions/{slug}", machine.address))
        .bearer_auth(&machine.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|err| format!("{} isn't reachable: {err}", machine.name))?;
    match res.status() {
        status if status.is_success() || status == reqwest::StatusCode::NOT_FOUND => Ok(()),
        status => Err(format!(
            "{} couldn't close the earlier session: {status}",
            machine.name
        )),
    }
}

async fn start_run(state: &AppState, task: &Task, run: &Slot, close: Option<&str>) -> Started {
    let Some(autostart) = &task.autostart else {
        return Started::Failed("it has no autostart".into());
    };
    if let Some(device) = close
        && let Err(err) = close_session(state, device, &task.slug).await
    {
        return Started::Failed(err);
    }
    let scheduled = ScheduledRun {
        occurrence: run.key(),
        complete_on_its_own: autostart.complete_on_its_own,
    };
    let req = StartAgent::scheduled(autostart);
    match agent::start_agent(state, task.slug.clone(), req, Some(scheduled)).await {
        Ok(started) if started["started"] == true => Started::Yes,
        Ok(_) => Started::Busy,
        Err((_, message)) => Started::Failed(message),
    }
}

/// A run that couldn't start, until it's tried again.
struct Waiting {
    run: Slot,
    retry_at: DateTime<Utc>,
}

/// Handle what has come; returns when to look again.
async fn tick(state: &AppState, waiting: &mut HashMap<String, Waiting>) -> Option<DateTime<Utc>> {
    let workspace = state.workspace.current()?;
    let ws = Arc::clone(&workspace);
    let tasks = tokio::task::spawn_blocking(move || ws.list_tasks(&TaskFilter::default()))
        .await
        .ok()?
        .ok()?;
    let activity = state.activity.all();
    let mut next: Option<DateTime<Utc>> = None;
    let mut sooner = |t: DateTime<Utc>| next = Some(next.map_or(t, |next| next.min(t)));
    let scheduled = tasks
        .iter()
        .filter(|task| task.autostart.is_some() && task.state != "completed");
    for task in scheduled {
        let now = Utc::now();
        let (run, missed, started) = match decide(task, activity.get(&task.slug), now) {
            Action::Wait { next } => {
                next.into_iter().for_each(&mut sooner);
                continue;
            }
            Action::Skip { run, missed } => (run, missed, false),
            Action::Start { run, missed, close } => {
                if let Some(wait) = waiting.get(&task.slug)
                    && wait.run == run
                    && wait.retry_at > now
                {
                    sooner(wait.retry_at);
                    continue;
                }
                match start_run(state, task, &run, close.as_deref()).await {
                    Started::Yes => {
                        waiting.remove(&task.slug);
                        (run, missed, true)
                    }
                    Started::Busy => (run, missed + 1, false),
                    Started::Failed(reason) => {
                        if waiting.get(&task.slug).is_none_or(|wait| wait.run != run) {
                            tracing::warn!(task = %task.slug, run = %run.key(), %reason, "couldn't start a scheduled run; trying again later");
                        }
                        let retry_at = now + RETRY;
                        sooner(retry_at);
                        waiting.insert(task.slug.clone(), Waiting { run, retry_at });
                        continue;
                    }
                }
            }
        };
        if started {
            tracing::info!(task = %task.slug, run = %run.key(), missed, "scheduled run started");
        } else {
            tracing::info!(task = %task.slug, run = %run.key(), missed, "scheduled run skipped: the previous one is still open");
        }
        let ws = Arc::clone(&workspace);
        let slug = task.slug.clone();
        let recorded = tokio::task::spawn_blocking(move || {
            ws.record_scheduled_run(&slug, &run, missed, started)
        })
        .await;
        if let Ok(Err(err)) = recorded {
            tracing::warn!(task = %task.slug, %err, "couldn't record a scheduled run");
        }
    }
    waiting.retain(|slug, _| tasks.iter().any(|task| &task.slug == slug));
    next
}

/// Whether an event can make a run due or able to start.
fn matters(event: &Event) -> bool {
    match event {
        Event::WorkspaceChanged { areas } => {
            areas.is_empty() || areas.iter().any(|area| area == "tasks")
        }
        Event::HomeChanged { .. } => true,
        Event::DeviceStatus { online, .. } => *online,
        _ => false,
    }
}

/// Start runs as they come due, while this core is the home.
pub fn spawn_scheduler(state: AppState) {
    let mut events = state.registry.subscribe();
    tokio::spawn(async move {
        tokio::time::sleep(SETTLE).await;
        let mut waiting = HashMap::new();
        loop {
            let next = if state.config.get().is_home() {
                tick(&state, &mut waiting).await
            } else {
                None
            };
            let sleep = next
                .and_then(|next| (next - Utc::now()).to_std().ok())
                .map_or(MAX_SLEEP, |until| until.min(MAX_SLEEP));
            let deadline = tokio::time::Instant::now() + sleep;
            loop {
                tokio::select! {
                    event = events.recv() => match event {
                        // A machine that's back can take the runs waiting for it
                        Ok(Event::DeviceStatus { online: true, .. }) => {
                            waiting.clear();
                            break;
                        }
                        Ok(event) if matters(&event) => break,
                        Ok(_) => {}
                        Err(RecvError::Lagged(_)) => break,
                        Err(RecvError::Closed) => return,
                    },
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::schedule::{Autostart, Zone};

    fn task(state: &str, due: &str, at: &str, recurrence: Option<&str>) -> Task {
        Task {
            slug: "check-reports".into(),
            state: state.into(),
            due: Some(due.into()),
            at: Some(at.into()),
            tz: Some("Europe/Oslo".into()),
            recurrence: recurrence.map(Into::into),
            autostart: Some(Autostart::default()),
            ..Task::default()
        }
    }

    fn slot(key: &str) -> Slot {
        Slot::from_key(key).expect("a key")
    }

    /// An instant given as Oslo's local time.
    fn at(key: &str) -> DateTime<Utc> {
        Zone::of(Some("Europe/Oslo"))
            .instant(&slot(key))
            .expect("an instant")
    }

    fn activity(state: &'static str, device: Option<&str>) -> Activity {
        Activity {
            state,
            seq: 1,
            device: device.map(Into::into),
            agent: device.map(|_| "claude".into()),
            status: None,
            reachable: device.map(|_| true),
        }
    }

    #[test]
    fn a_task_should_wait_for_its_time() {
        let task = task("not-started", "2026-11-16", "09:00", Some("weekdays"));
        assert_eq!(
            decide(&task, None, at("2026-11-16 08:59")),
            Action::Wait {
                next: Some(at("2026-11-16 09:00"))
            }
        );
    }

    #[test]
    fn a_task_that_came_should_start() {
        let task = task("not-started", "2026-11-16", "09:00", Some("weekdays"));
        assert_eq!(
            decide(&task, None, at("2026-11-16 09:00")),
            Action::Start {
                run: slot("2026-11-16 09:00"),
                missed: 0,
                close: None
            }
        );
    }

    #[test]
    fn an_occurrence_that_was_handled_should_not_start_again() {
        let mut task = task("not-started", "2026-11-16", "09:00", Some("daily"));
        task.last_run = Some("2026-11-16 09:00".into());
        assert_eq!(
            decide(&task, None, at("2026-11-16 12:00")),
            Action::Wait {
                next: Some(at("2026-11-17 09:00"))
            }
        );
    }

    #[test]
    fn a_run_should_be_skipped_while_the_previous_one_is_open() {
        let mut task = task("in-progress", "2026-11-16", "09:00", Some("daily"));
        task.last_run = Some("2026-11-16 09:00".into());
        assert_eq!(
            decide(
                &task,
                Some(&activity("ready", Some("mac"))),
                at("2026-11-18 09:30")
            ),
            Action::Skip {
                run: slot("2026-11-18 09:00"),
                missed: 2
            }
        );
    }

    #[test]
    fn a_run_should_be_skipped_while_someone_works_in_its_session() {
        let task = task("not-started", "2026-11-16", "09:00", Some("daily"));
        assert_eq!(
            decide(
                &task,
                Some(&activity("working", Some("mac"))),
                at("2026-11-16 09:00")
            ),
            Action::Skip {
                run: slot("2026-11-16 09:00"),
                missed: 1
            }
        );
    }

    #[test]
    fn an_idle_session_from_the_last_run_should_be_closed_first() {
        let mut task = task("not-started", "2026-11-17", "09:00", Some("daily"));
        task.last_run = Some("2026-11-16 09:00".into());
        assert_eq!(
            decide(
                &task,
                Some(&activity("ready", Some("mac"))),
                at("2026-11-17 09:01")
            ),
            Action::Start {
                run: slot("2026-11-17 09:00"),
                missed: 0,
                close: Some("mac".into())
            }
        );
    }

    #[test]
    fn missed_occurrences_should_come_to_one_run() {
        let mut task = task("not-started", "2026-11-12", "09:00", Some("weekdays"));
        task.last_run = Some("2026-11-11 09:00".into());
        // Thursday and Friday passed while the home was down
        assert_eq!(
            decide(&task, None, at("2026-11-16 10:00")),
            Action::Start {
                run: slot("2026-11-16 09:00"),
                missed: 2,
                close: None
            }
        );
    }

    #[test]
    fn a_task_that_doesn_t_repeat_should_run_once() {
        let mut task = task("not-started", "2026-11-16", "09:00", None);
        assert!(matches!(
            decide(&task, None, at("2026-11-16 09:00")),
            Action::Start { missed: 0, .. }
        ));
        task.last_run = Some("2026-11-16 09:00".into());
        assert_eq!(
            decide(&task, None, at("2026-11-20 09:00")),
            Action::Wait { next: None }
        );
    }

    #[test]
    fn only_task_changes_and_returning_machines_should_wake_it() {
        let tasks = Event::WorkspaceChanged {
            areas: vec!["tasks".into()],
        };
        let plans = Event::WorkspaceChanged {
            areas: vec!["plans".into()],
        };
        let back = Event::DeviceStatus {
            device: "mac".into(),
            online: true,
        };
        let gone = Event::DeviceStatus {
            device: "mac".into(),
            online: false,
        };
        assert!(matters(&tasks));
        assert!(!matters(&plans));
        assert!(matters(&back));
        assert!(!matters(&gone));
    }
}
