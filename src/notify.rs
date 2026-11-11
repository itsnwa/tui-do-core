//! Phone notifications when an agent needs someone (sent with `apns`).
//!
//! The home core sees every session's status: its own, and other machines'
//! as relayed `deviceEvent`s. A session whose agent starts asking for
//! something, fails or finishes its turn, and still stands there a little
//! later, is notified once, to each paired client that turned notifications
//! on, unless that client has the session open or someone started typing an
//! answer. A turn the user interrupted isn't news to them, and one that left
//! agents of its own running comes back by itself.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::session::{AgentState, Event};
use crate::status::{State, Status};

/// How long a session has to stay waiting before it's notified.
pub const DELAY: Duration = Duration::from_secs(15);

/// A session on a machine: (device id, session id).
pub type SessionKey = (String, String);

/// What the notifier needs from an event.
#[derive(Debug, PartialEq)]
pub enum Change {
    /// The session started calling for someone; `body` is what to tell them.
    Calls { key: SessionKey, body: String },
    /// It stopped calling: it works again, someone is typing an answer, or
    /// the session ended.
    Settled(SessionKey),
}

/// What to tell someone about a status that calls for them.
fn body(status: &Status) -> String {
    let fallback = match status.state {
        State::Blocked => "Waiting for your answer",
        State::Failed => "Stopped on an error",
        _ => "Finished its turn",
    };
    status
        .detail
        .clone()
        .unwrap_or_else(|| fallback.to_string())
}

fn of_status(key: SessionKey, status: &Status) -> Change {
    if status.view().calls_for_attention() {
        Change::Calls {
            key,
            body: body(status),
        }
    } else {
        Change::Settled(key)
    }
}

/// The change an event makes, given this core's own device id.
pub fn change(event: &Event, own_id: &str) -> Option<Change> {
    match event {
        Event::SessionStatus { id, status } => {
            Some(of_status((own_id.to_string(), id.to_string()), status))
        }
        Event::SessionTyped { id }
        | Event::SessionExited { id, .. }
        | Event::SessionRemoved { id } => {
            Some(Change::Settled((own_id.to_string(), id.to_string())))
        }
        Event::DeviceEvent { device, event } => {
            let id = event["id"].as_str()?;
            let key = (device.clone(), id.to_string());
            match event["type"].as_str()? {
                "sessionStatus" => {
                    let status = serde_json::from_value(event["status"].clone()).ok()?;
                    Some(of_status(key, &status))
                }
                // With `seq` it repeats a status that was already taken in
                "agentState" if event["seq"].is_null() => {
                    // A core from before statuses: waiting is all it says
                    match serde_json::from_value(event["state"].clone()).ok()? {
                        AgentState::Waiting => Some(Change::Calls {
                            key,
                            body: event["detail"]
                                .as_str()
                                .unwrap_or("Waiting for input")
                                .into(),
                        }),
                        AgentState::Idle | AgentState::Working => Some(Change::Settled(key)),
                    }
                }
                "sessionTyped" | "sessionExited" | "sessionRemoved" => Some(Change::Settled(key)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Sessions about to be notified, each with when and what to say.
#[derive(Default)]
pub struct Waits {
    pending: HashMap<SessionKey, (Instant, String)>,
}

impl Waits {
    pub fn apply(&mut self, change: Change, now: Instant) {
        match change {
            Change::Calls { key, body } => {
                self.pending.insert(key, (now + DELAY, body));
            }
            Change::Settled(key) => {
                self.pending.remove(&key);
            }
        }
    }

    /// When the next one is due.
    pub fn next(&self) -> Option<Instant> {
        self.pending.values().map(|(at, _)| *at).min()
    }

    /// The sessions due by `now`, which won't be notified again until they
    /// call for someone again.
    pub fn take_due(&mut self, now: Instant) -> Vec<(SessionKey, String)> {
        let due: Vec<SessionKey> = self
            .pending
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        due.into_iter()
            .filter_map(|key| {
                let (_, body) = self.pending.remove(&key)?;
                Some((key, body))
            })
            .collect()
    }
}

/// Which paired clients have which sessions open, so a client isn't told
/// about the session it's looking at.
#[derive(Default)]
pub struct Watching {
    open: Mutex<HashMap<(String, SessionKey), usize>>,
}

/// Held while a client has a session open.
pub struct Watch {
    watching: Arc<Watching>,
    key: (String, SessionKey),
}

impl Watching {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(String, SessionKey), usize>> {
        self.open.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn open(self: &Arc<Self>, client: &str, session: SessionKey) -> Watch {
        let key = (client.to_string(), session);
        *self.lock().entry(key.clone()).or_default() += 1;
        Watch {
            watching: Arc::clone(self),
            key,
        }
    }

    pub fn is_open(&self, client: &str, session: &SessionKey) -> bool {
        self.lock()
            .contains_key(&(client.to_string(), session.clone()))
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut open = self.watching.lock();
        if let Some(count) = open.get_mut(&self.key) {
            *count -= 1;
            if *count == 0 {
                open.remove(&self.key);
            }
        }
    }
}

/// The app's link to a session.
pub fn session_link(device: &str, session: &str) -> String {
    let encode = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    char::from(b).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    format!("tuido://session/{}/{}", encode(device), encode(session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::{Source, View};
    use std::sync::Arc;

    fn key(id: &str) -> SessionKey {
        ("cube".to_string(), id.to_string())
    }

    fn calls(id: &str) -> Change {
        Change::Calls {
            key: key(id),
            body: "Which one?".into(),
        }
    }

    fn status(state: State, reason: &str, detail: Option<&str>) -> Status {
        let view = View::new(state, Some(reason), detail.map(str::to_string));
        Status::next(view, Source::Hooks, 0, None)
    }

    #[test]
    fn a_session_still_calling_after_the_delay_should_be_due_once() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(calls("a"), start);
        assert!(waits.take_due(start + DELAY / 2).is_empty());
        assert_eq!(waits.next(), Some(start + DELAY));
        let due = waits.take_due(start + DELAY);
        assert_eq!(due, vec![(key("a"), "Which one?".to_string())]);
        assert!(waits.take_due(start + DELAY * 10).is_empty());
        assert_eq!(waits.next(), None);
    }

    #[test]
    fn a_session_that_stops_calling_should_not_be_due() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(calls("a"), start);
        waits.apply(Change::Settled(key("a")), start);
        assert!(waits.take_due(start + DELAY).is_empty());
    }

    #[test]
    fn a_session_someone_answers_should_not_be_due() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(calls("a"), start);
        waits.apply(calls("b"), start);
        let typed = Event::DeviceEvent {
            device: "cube".into(),
            event: serde_json::json!({ "type": "sessionTyped", "id": "a" }),
        };
        waits.apply(change(&typed, "mac").expect("a change"), start + DELAY / 2);
        let due = waits.take_due(start + DELAY);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, key("b"));
    }

    /// The change a status makes on this core, as (calls, body).
    fn of(status: Status) -> (bool, Option<String>) {
        let event = Event::SessionStatus {
            id: "task".into(),
            status,
        };
        match change(&event, "mac").expect("a change") {
            Change::Calls { key, body } => {
                assert_eq!(key, ("mac".to_string(), "task".to_string()));
                (true, Some(body))
            }
            Change::Settled(_) => (false, None),
        }
    }

    #[test]
    fn a_status_should_call_for_someone_when_it_needs_them() {
        let says = |text: &str| (true, Some(text.to_string()));
        assert_eq!(
            of(status(
                State::Blocked,
                "permission",
                Some("Allow Bash: ls?")
            )),
            says("Allow Bash: ls?")
        );
        assert_eq!(
            of(status(State::Blocked, "question", None)),
            says("Waiting for your answer")
        );
        assert_eq!(
            of(status(State::Failed, "rate_limit", None)),
            says("Stopped on an error")
        );
        assert_eq!(
            of(status(State::Ready, "finished", Some("Done."))),
            says("Done.")
        );
        assert_eq!(
            of(status(State::Ready, "quiet", None)),
            says("Finished its turn")
        );
    }

    #[test]
    fn a_status_nobody_has_to_act_on_should_settle() {
        for quiet in [
            status(State::Working, "turn", None),
            status(State::Ready, "interrupted", None),
            status(State::Ready, "background", Some("Agents are on it.")),
            status(State::Ready, "started", None),
            status(State::Ended, "", None),
        ] {
            assert_eq!(of(quiet), (false, None));
        }
    }

    fn relayed(event: serde_json::Value) -> Option<Change> {
        let event = Event::DeviceEvent {
            device: "cube".into(),
            event,
        };
        change(&event, "mac")
    }

    #[test]
    fn a_device_s_events_should_become_changes() {
        let blocked = status(State::Blocked, "question", Some("Go?"));
        let event = serde_json::json!({ "type": "sessionStatus", "id": "t", "status": blocked });
        assert_eq!(
            relayed(event),
            Some(Change::Calls {
                key: key("t"),
                body: "Go?".into(),
            })
        );
        let removed = serde_json::json!({ "type": "sessionRemoved", "id": "t" });
        assert_eq!(relayed(removed), Some(Change::Settled(key("t"))));
        let output = serde_json::json!({ "type": "output", "id": "t", "data": "x" });
        assert_eq!(relayed(output), None);
    }

    #[test]
    fn an_agent_state_should_only_count_from_a_core_without_statuses() {
        // An older core: its three states are all there is
        let waiting = serde_json::json!({ "type": "agentState", "id": "t", "state": "waiting", "detail": "Go?" });
        assert_eq!(
            relayed(waiting),
            Some(Change::Calls {
                key: key("t"),
                body: "Go?".into(),
            })
        );
        let bare = serde_json::json!({ "type": "agentState", "id": "t", "state": "waiting" });
        assert_eq!(
            relayed(bare),
            Some(Change::Calls {
                key: key("t"),
                body: "Waiting for input".into(),
            })
        );
        let working = serde_json::json!({ "type": "agentState", "id": "t", "state": "working" });
        assert_eq!(relayed(working), Some(Change::Settled(key("t"))));

        // A newer core sent the status first; this only repeats it. An
        // interrupted turn reads as idle here, and must not undo anything
        let repeat =
            serde_json::json!({ "type": "agentState", "id": "t", "state": "waiting", "seq": 7 });
        assert_eq!(relayed(repeat), None);
        let own = Event::AgentState {
            id: "task".into(),
            state: AgentState::Waiting,
            detail: Some("Go?".into()),
            seq: Some(7),
        };
        assert_eq!(change(&own, "mac"), None);
    }

    #[test]
    fn a_session_should_count_as_open_until_every_watch_ends() {
        let watching = Arc::new(Watching::default());
        let first = watching.open("phone", key("a"));
        let second = watching.open("phone", key("a"));
        assert!(watching.is_open("phone", &key("a")));
        assert!(!watching.is_open("ipad", &key("a")));
        assert!(!watching.is_open("phone", &key("b")));
        drop(first);
        assert!(watching.is_open("phone", &key("a")));
        drop(second);
        assert!(!watching.is_open("phone", &key("a")));
    }

    #[test]
    fn session_link_should_encode_its_parts() {
        assert_eq!(
            session_link("abc-1", "26-962 x/y"),
            "tuido://session/abc-1/26-962%20x%2Fy"
        );
    }
}
