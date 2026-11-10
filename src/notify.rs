//! Phone notifications when an agent waits for input (sent with `apns`).
//!
//! The home core sees every session's agent state: its own, and other
//! machines' as relayed `deviceEvent`s. A session that turns `waiting` and is
//! still waiting a little later is notified once, to each paired client that
//! turned notifications on, unless that client has the session open or
//! someone was typing in it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::time::Instant;

use crate::session::{AgentState, Event};

/// How long a session has to stay waiting before it's notified.
pub const DELAY: Duration = Duration::from_secs(15);
/// Someone who typed this recently before the agent started waiting is at it.
pub const TYPED_RECENTLY: Duration = Duration::from_secs(30);

/// A session on a machine: (device id, session id).
pub type SessionKey = (String, String);

/// What the notifier needs from an event.
#[derive(Debug, PartialEq)]
pub enum Change {
    State {
        key: SessionKey,
        state: AgentState,
        detail: Option<String>,
        input_ago: Option<Duration>,
    },
    Gone(SessionKey),
}

/// The change an event makes, given this core's own device id.
pub fn change(event: &Event, own_id: &str) -> Option<Change> {
    match event {
        Event::AgentState {
            id,
            state,
            detail,
            input_ago_ms,
        } => Some(Change::State {
            key: (own_id.to_string(), id.to_string()),
            state: *state,
            detail: detail.clone(),
            input_ago: input_ago_ms.map(Duration::from_millis),
        }),
        Event::SessionExited { id, .. } | Event::SessionRemoved { id } => {
            Some(Change::Gone((own_id.to_string(), id.to_string())))
        }
        Event::DeviceEvent { device, event } => {
            let id = event["id"].as_str()?;
            let key = (device.clone(), id.to_string());
            match event["type"].as_str()? {
                "agentState" => Some(Change::State {
                    key,
                    state: serde_json::from_value(event["state"].clone()).ok()?,
                    detail: event["detail"].as_str().map(str::to_string),
                    input_ago: event["inputAgoMs"].as_u64().map(Duration::from_millis),
                }),
                "sessionExited" | "sessionRemoved" => Some(Change::Gone(key)),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Sessions waiting to be notified, each with when and what it waits for.
#[derive(Default)]
pub struct Waits {
    pending: HashMap<SessionKey, (Instant, Option<String>)>,
}

impl Waits {
    pub fn apply(&mut self, change: Change, now: Instant) {
        match change {
            Change::State {
                key,
                state: AgentState::Waiting,
                detail,
                input_ago,
            } => {
                if input_ago.is_some_and(|ago| ago < TYPED_RECENTLY) {
                    self.pending.remove(&key);
                } else {
                    self.pending.insert(key, (now + DELAY, detail));
                }
            }
            Change::State { key, .. } | Change::Gone(key) => {
                self.pending.remove(&key);
            }
        }
    }

    /// When the next one is due.
    pub fn next(&self) -> Option<Instant> {
        self.pending.values().map(|(at, _)| *at).min()
    }

    /// The sessions due by `now`, which won't be notified again until they
    /// start waiting again.
    pub fn take_due(&mut self, now: Instant) -> Vec<(SessionKey, Option<String>)> {
        let due: Vec<SessionKey> = self
            .pending
            .iter()
            .filter(|(_, (at, _))| *at <= now)
            .map(|(key, _)| key.clone())
            .collect();
        due.into_iter()
            .filter_map(|key| {
                let (_, detail) = self.pending.remove(&key)?;
                Some((key, detail))
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
    use std::sync::Arc;

    fn key(id: &str) -> SessionKey {
        ("cube".to_string(), id.to_string())
    }

    fn state(id: &str, state: AgentState, input_ago: Option<u64>) -> Change {
        Change::State {
            key: key(id),
            state,
            detail: Some("Which one?".into()),
            input_ago: input_ago.map(Duration::from_secs),
        }
    }

    #[test]
    fn a_session_still_waiting_after_the_delay_should_be_due_once() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(state("a", AgentState::Waiting, None), start);
        assert!(waits.take_due(start + DELAY / 2).is_empty());
        assert_eq!(waits.next(), Some(start + DELAY));
        let due = waits.take_due(start + DELAY);
        assert_eq!(due, vec![(key("a"), Some("Which one?".into()))]);
        assert!(waits.take_due(start + DELAY * 10).is_empty());
        assert_eq!(waits.next(), None);
    }

    #[test]
    fn a_session_that_stops_waiting_should_not_be_due() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(state("a", AgentState::Waiting, None), start);
        waits.apply(state("a", AgentState::Working, None), start);
        waits.apply(state("b", AgentState::Waiting, None), start);
        waits.apply(Change::Gone(key("b")), start);
        assert!(waits.take_due(start + DELAY).is_empty());
    }

    #[test]
    fn a_session_someone_just_typed_in_should_not_be_due() {
        let start = Instant::now();
        let mut waits = Waits::default();
        waits.apply(state("a", AgentState::Waiting, Some(5)), start);
        waits.apply(state("b", AgentState::Waiting, Some(600)), start);
        let due = waits.take_due(start + DELAY);
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].0, key("b"));
    }

    #[test]
    fn events_from_this_core_and_devices_should_become_changes() {
        let own = Event::AgentState {
            id: "task".into(),
            state: AgentState::Waiting,
            detail: Some("Go?".into()),
            input_ago_ms: Some(1500),
        };
        assert_eq!(
            change(&own, "mac"),
            Some(Change::State {
                key: ("mac".into(), "task".into()),
                state: AgentState::Waiting,
                detail: Some("Go?".into()),
                input_ago: Some(Duration::from_millis(1500)),
            })
        );
        let relayed = Event::DeviceEvent {
            device: "cube".into(),
            event: serde_json::json!({ "type": "agentState", "id": "t", "state": "working" }),
        };
        assert_eq!(
            change(&relayed, "mac"),
            Some(Change::State {
                key: key("t"),
                state: AgentState::Working,
                detail: None,
                input_ago: None,
            })
        );
        let removed = Event::DeviceEvent {
            device: "cube".into(),
            event: serde_json::json!({ "type": "sessionRemoved", "id": "t" }),
        };
        assert_eq!(change(&removed, "mac"), Some(Change::Gone(key("t"))));
        let output = Event::DeviceEvent {
            device: "cube".into(),
            event: serde_json::json!({ "type": "output", "id": "t", "data": "x" }),
        };
        assert_eq!(change(&output, "mac"), None);
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
