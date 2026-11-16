//! Pushes that follow agents, sent with `apns` by `api::notifications`.
//!
//! A paired client's widgets load again when what a task shows changes in a
//! way they show: an agent starts, stops, asks or finishes, or a task under
//! way is done. And each agent at work has a Live Activity on the clients
//! that turned them on: the home core starts it (push-to-start) when the
//! agent gets going or calls for someone, updates it when the agent does or
//! asks something else, and ends it when the agent stops, or has rested
//! after its turn for [`REST`]. [`Followed`] works out which pushes a change
//! calls for; the payloads are built here too.

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::time::Instant;

use crate::activity::Activity;

/// What a Live Activity follows: an agent at work, or one whose turn is
/// over and waits for someone.
const LIVE: [&str; 4] = ["needs-you", "error", "working", "ready"];

/// How long a Live Activity stays up after its agent's turn is over.
pub const REST: Duration = Duration::from_secs(10 * 60);

/// What the app's Live Activity shows of an agent (its `ContentState`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Shown {
    /// The task's word.
    pub state: &'static str,
    /// The question, the error or the last line of its reply, while it
    /// calls for someone.
    pub detail: Option<String>,
}

impl Shown {
    pub fn content_state(&self) -> Value {
        match &self.detail {
            Some(detail) => json!({ "state": self.state, "detail": detail }),
            None => json!({ "state": self.state }),
        }
    }

    /// 10 when someone should see it now, 5 while it only works on.
    pub fn priority(&self) -> u8 {
        if self.state == "working" { 5 } else { 10 }
    }

    /// The word as the apps say it.
    pub fn label(&self) -> &'static str {
        match self.state {
            "needs-you" => "needs you",
            "not-started" => "not started",
            word => word,
        }
    }
}

/// A task's agent, as its Live Activity follows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    /// The machine its session is on.
    pub device: String,
    /// The task's slug, which is the session's id.
    pub task: String,
    pub shown: Shown,
}

impl Agent {
    /// The session it follows, as clients' activity tokens are kept by.
    pub fn key(&self) -> String {
        key(&self.device, &self.task)
    }

    /// It gets a Live Activity started: it works or calls for someone. One
    /// whose turn is over only keeps the one it has.
    pub fn starts(&self) -> bool {
        self.shown.state != "ready"
    }
}

pub fn key(device: &str, session: &str) -> String {
    format!("{device}/{session}")
}

/// The agent a task's activity says is at work, if a Live Activity follows it.
pub fn agent(task: &str, activity: Option<&Activity>) -> Option<Agent> {
    let activity = activity?;
    let state = *LIVE.iter().find(|word| **word == activity.state)?;
    activity.agent.as_ref()?;
    let status = activity.status.as_ref();
    let detail = status
        .filter(|status| status.view().calls_for_attention())
        .and_then(|status| status.detail.clone());
    Some(Agent {
        device: activity.device.clone()?,
        task: task.to_string(),
        shown: Shown { state, detail },
    })
}

/// The task's agent is done with its turn and nothing of its own runs on.
fn rests(activity: Option<&Activity>) -> bool {
    activity.is_some_and(|a| {
        a.state == "ready"
            && a.status
                .as_ref()
                .is_none_or(|s| s.view().reason.as_deref() != Some("background"))
    })
}

/// A push a change calls for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Push {
    /// The widgets should load again.
    Widgets,
    /// An agent got going: start its Live Activity.
    Start(Agent),
    /// It does or asks something else.
    Update(Agent),
    /// It stopped, or its session moved: end the Live Activity that followed
    /// the session `key`, showing `shown` last.
    End { key: String, shown: Shown },
}

/// What the pushes went by, so far: each task's word as the widgets show
/// it, the agents Live Activities follow, and since when the ones whose turn
/// is over rest.
#[derive(Default)]
pub struct Followed {
    words: HashMap<String, &'static str>,
    agents: HashMap<String, Agent>,
    resting: HashMap<String, Instant>,
}

impl Followed {
    /// The pushes a task's new activity calls for (none for a task that's
    /// gone or completed without an agent left).
    pub fn change(&mut self, task: &str, activity: Option<&Activity>, now: Instant) -> Vec<Push> {
        let mut pushes = Vec::new();
        // A task nobody started looks the same to the widgets as one that
        // isn't there; one under way that goes away was done (or deleted)
        let word = activity
            .map(|a| a.state)
            .filter(|word| *word != "not-started");
        let was = match word {
            Some(word) => self.words.insert(task.to_string(), word),
            None => self.words.remove(task),
        };
        if was != word {
            pushes.push(Push::Widgets);
        }
        let last = |was: &Agent| Shown {
            // What it came to, if the task still says
            state: activity.map_or(was.shown.state, |a| a.state),
            detail: None,
        };
        let agent = agent(task, activity);
        let followed = agent.is_some();
        match (self.agents.remove(task), agent) {
            (None, None) => {}
            (None, Some(agent)) => {
                if agent.starts() {
                    pushes.push(Push::Start(agent.clone()));
                }
                self.agents.insert(task.to_string(), agent);
            }
            (Some(was), None) => pushes.push(Push::End {
                key: was.key(),
                shown: last(&was),
            }),
            (Some(was), Some(agent)) => {
                if was.device != agent.device {
                    pushes.push(Push::End {
                        key: was.key(),
                        shown: was.shown.clone(),
                    });
                    if agent.starts() {
                        pushes.push(Push::Start(agent.clone()));
                    }
                } else if was.shown != agent.shown {
                    // Back at work after a rest, its activity may have ended
                    if !was.starts() && agent.starts() {
                        pushes.push(Push::Start(agent.clone()));
                    } else {
                        pushes.push(Push::Update(agent.clone()));
                    }
                }
                self.agents.insert(task.to_string(), agent);
            }
        }
        if followed && rests(activity) {
            self.resting.entry(task.to_string()).or_insert(now);
        } else {
            self.resting.remove(task);
        }
        pushes
    }

    /// When the next resting agent's Live Activity is due to end.
    pub fn next_rest(&self) -> Option<Instant> {
        self.resting.values().map(|since| *since + REST).min()
    }

    /// End the Live Activities of agents that have rested for `REST` by
    /// `now`; they start again when their agent gets going.
    pub fn retire(&mut self, now: Instant) -> Vec<Push> {
        let due: Vec<String> = self
            .resting
            .iter()
            .filter(|(_, since)| now >= **since + REST)
            .map(|(task, _)| task.clone())
            .collect();
        due.into_iter()
            .filter_map(|task| {
                self.resting.remove(&task);
                let agent = self.agents.get(&task)?;
                Some(Push::End {
                    key: agent.key(),
                    shown: agent.shown.clone(),
                })
            })
            .collect()
    }

    /// The agents that should have a Live Activity now.
    pub fn starting(&self) -> Vec<Agent> {
        self.agents
            .values()
            .filter(|a| a.starts())
            .cloned()
            .collect()
    }
}

/// What a started Live Activity says of its task and machine (its
/// `AgentAttributes` in the app).
pub struct About {
    /// The task's short id, `26-1075`.
    pub id: String,
    pub title: String,
    pub machine: String,
}

/// Start a Live Activity for an agent (sent to a push-to-start token). It
/// asks for the activity's own token too, which the app hands back for the
/// updates and the end: without `input-push-token`, iOS 18 gives it none.
pub fn start_payload(agent: &Agent, about: &About, timestamp: u64) -> Value {
    json!({
        "aps": {
            "timestamp": timestamp,
            "event": "start",
            "input-push-token": 1,
            "content-state": agent.shown.content_state(),
            "attributes-type": "AgentAttributes",
            "attributes": {
                "device": agent.device,
                "session": agent.task,
                "task": about.id,
                "title": about.title,
                "machine": about.machine,
            },
            "alert": {
                "title": about.title,
                "body": format!("{} on {}", agent.shown.label(), about.machine),
            },
        }
    })
}

/// Update a Live Activity, and have it alert: light up the Lock Screen or
/// expand in the Dynamic Island, with a sound, instead of a notification.
pub fn alert_payload(shown: &Shown, title: &str, body: &str, timestamp: u64) -> Value {
    let mut payload = update_payload(shown, timestamp);
    payload["aps"]["alert"] = json!({ "title": title, "body": body, "sound": "default" });
    payload
}

pub fn update_payload(shown: &Shown, timestamp: u64) -> Value {
    json!({
        "aps": {
            "timestamp": timestamp,
            "event": "update",
            "content-state": shown.content_state(),
        }
    })
}

/// End a Live Activity, and take it off the Lock Screen right away.
pub fn end_payload(shown: &Shown, timestamp: u64) -> Value {
    json!({
        "aps": {
            "timestamp": timestamp,
            "event": "end",
            "content-state": shown.content_state(),
            "dismissal-date": timestamp,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Source, State, Status, View};

    fn activity(state: &'static str, status: Option<(State, &str, Option<&str>)>) -> Activity {
        let status = status.map(|(state, reason, detail)| {
            let view = View::new(state, Some(reason), detail.map(str::to_string));
            Status::next(view, Source::Hooks, 0, None)
        });
        Activity {
            state,
            seq: 1,
            device: status.as_ref().map(|_| "cube".to_string()),
            agent: status.as_ref().map(|_| "claude".to_string()),
            status,
            reachable: Some(true),
        }
    }

    fn working() -> Activity {
        activity("working", Some((State::Working, "turn", None)))
    }

    fn asking(detail: &str) -> Activity {
        activity(
            "needs-you",
            Some((State::Blocked, "permission", Some(detail))),
        )
    }

    fn shown(state: &'static str, detail: Option<&str>) -> Shown {
        Shown {
            state,
            detail: detail.map(str::to_string),
        }
    }

    fn agent(state: &'static str, detail: Option<&str>) -> Agent {
        Agent {
            device: "cube".into(),
            task: "fix".into(),
            shown: shown(state, detail),
        }
    }

    impl Followed {
        fn change_now(&mut self, task: &str, activity: Option<&Activity>) -> Vec<Push> {
            self.change(task, activity, Instant::now())
        }
    }

    fn finished() -> Activity {
        activity("ready", Some((State::Ready, "finished", Some("Done."))))
    }

    #[test]
    fn an_agent_should_be_followed_from_start_to_end() {
        let mut followed = Followed::default();
        assert_eq!(
            followed.change_now("fix", Some(&working())),
            vec![Push::Widgets, Push::Start(agent("working", None))]
        );
        // Nothing new: no push
        assert_eq!(followed.change_now("fix", Some(&working())), vec![]);
        assert_eq!(
            followed.change_now("fix", Some(&asking("Allow Bash?"))),
            vec![
                Push::Widgets,
                Push::Update(agent("needs-you", Some("Allow Bash?")))
            ]
        );
        // Another question: the Live Activity says it, the widgets don't care
        assert_eq!(
            followed.change_now("fix", Some(&asking("Allow Edit?"))),
            vec![Push::Update(agent("needs-you", Some("Allow Edit?")))]
        );
        // The agent exits: the task is paused
        assert_eq!(
            followed.change_now("fix", Some(&activity("paused", None))),
            vec![
                Push::Widgets,
                Push::End {
                    key: "cube/fix".into(),
                    shown: shown("paused", None)
                }
            ]
        );
        // Done, with no session left
        assert_eq!(followed.change_now("fix", None), vec![Push::Widgets]);
    }

    #[test]
    fn a_task_nobody_started_should_not_move_the_widgets() {
        let mut followed = Followed::default();
        assert_eq!(
            followed.change_now("new", Some(&activity("not-started", None))),
            vec![]
        );
        assert_eq!(followed.change_now("new", None), vec![]);
    }

    #[test]
    fn a_finished_turn_should_say_its_last_line() {
        let mut followed = Followed::default();
        followed.change_now("fix", Some(&working()));
        assert_eq!(
            followed.change_now("fix", Some(&finished())),
            vec![Push::Widgets, Push::Update(agent("ready", Some("Done.")))]
        );
        // Ready with agents of its own running doesn't call for anyone
        let background = activity(
            "ready",
            Some((State::Ready, "background", Some("Agents are on it."))),
        );
        assert_eq!(
            followed.change_now("fix", Some(&background)),
            vec![Push::Update(agent("ready", None))]
        );
    }

    #[test]
    fn an_agent_whose_turn_is_over_should_not_get_an_activity_started() {
        let mut followed = Followed::default();
        assert_eq!(
            followed.change_now("fix", Some(&finished())),
            vec![Push::Widgets]
        );
        assert_eq!(followed.starting(), vec![]);
        assert_eq!(
            followed.change_now("fix", Some(&working())),
            vec![Push::Widgets, Push::Start(agent("working", None))]
        );
        assert_eq!(followed.starting(), vec![agent("working", None)]);
    }

    #[test]
    fn a_resting_agent_s_activity_should_end_after_a_while_and_start_again_at_work() {
        let start = Instant::now();
        let mut followed = Followed::default();
        followed.change("fix", Some(&working()), start);
        assert_eq!(followed.next_rest(), None);
        followed.change("fix", Some(&finished()), start);
        // Still resting: the rest goes on from when it began
        let quiet = activity("ready", Some((State::Ready, "quiet", None)));
        followed.change("fix", Some(&quiet), start + REST / 2);
        assert_eq!(followed.next_rest(), Some(start + REST));
        assert_eq!(followed.retire(start + REST / 2), vec![]);
        assert_eq!(
            followed.retire(start + REST),
            vec![Push::End {
                key: "cube/fix".into(),
                shown: shown("ready", None)
            }]
        );
        assert_eq!(followed.next_rest(), None);
        assert_eq!(
            followed.change("fix", Some(&working()), start + REST * 2),
            vec![Push::Widgets, Push::Start(agent("working", None))]
        );
    }

    #[test]
    fn an_agent_with_agents_of_its_own_running_should_not_rest() {
        let mut followed = Followed::default();
        followed.change_now("fix", Some(&working()));
        let background = activity("ready", Some((State::Ready, "background", None)));
        followed.change_now("fix", Some(&background));
        assert_eq!(followed.next_rest(), None);
        followed.change_now("fix", Some(&finished()));
        assert!(followed.next_rest().is_some());
        followed.change_now("fix", Some(&working()));
        assert_eq!(followed.next_rest(), None);
    }

    #[test]
    fn a_session_that_moves_should_end_one_activity_and_start_another() {
        let mut followed = Followed::default();
        followed.change_now("fix", Some(&working()));
        let mut elsewhere = working();
        elsewhere.device = Some("mbp".into());
        let moved = Agent {
            device: "mbp".into(),
            ..agent("working", None)
        };
        assert_eq!(
            followed.change_now("fix", Some(&elsewhere)),
            vec![
                Push::End {
                    key: "cube/fix".into(),
                    shown: shown("working", None)
                },
                Push::Start(moved)
            ]
        );
    }

    #[test]
    fn payloads_should_carry_what_the_app_reads() {
        let agent = agent("needs-you", Some("Allow Bash?"));
        let about = About {
            id: "26-1075".into(),
            title: "iOS Widgets".into(),
            machine: "cube".into(),
        };
        assert_eq!(
            start_payload(&agent, &about, 1_700_000_000),
            json!({
                "aps": {
                    "timestamp": 1_700_000_000,
                    "event": "start",
                    "input-push-token": 1,
                    "content-state": { "state": "needs-you", "detail": "Allow Bash?" },
                    "attributes-type": "AgentAttributes",
                    "attributes": {
                        "device": "cube", "session": "fix", "task": "26-1075",
                        "title": "iOS Widgets", "machine": "cube",
                    },
                    "alert": { "title": "iOS Widgets", "body": "needs you on cube" },
                }
            })
        );
        assert_eq!(
            alert_payload(&agent.shown, "iOS Widgets", "Allow Bash?", 8),
            json!({
                "aps": {
                    "timestamp": 8,
                    "event": "update",
                    "content-state": { "state": "needs-you", "detail": "Allow Bash?" },
                    "alert": { "title": "iOS Widgets", "body": "Allow Bash?", "sound": "default" },
                }
            })
        );
        let working = shown("working", None);
        assert_eq!(
            update_payload(&working, 7),
            json!({ "aps": { "timestamp": 7, "event": "update", "content-state": { "state": "working" } } })
        );
        assert_eq!(working.priority(), 5);
        assert_eq!(agent.shown.priority(), 10);
        assert_eq!(
            end_payload(&shown("done", None), 9)["aps"]["dismissal-date"],
            9
        );
    }
}
