//! What each task shows: one word, worked out by the home core from the
//! task's own state and the status of its agent's session, wherever in the
//! fleet that runs. Clients draw it as it is, so the Mac and the phone can't
//! tell two stories about the same task.
//!
//! The home hears every machine's session events through its relays and
//! keeps them in a [`Mirror`]. The [`Board`] joins that with the tasks and
//! says what changed.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use serde::Serialize;
use serde_json::Value;

use crate::session::{AgentState, Event, SessionInfo};
use crate::status::{self, Source, State, Status};

/// What a task shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Activity {
    /// `needs-you`, `error`, `working`, `ready`, `paused`, `not-started` or
    /// `done`.
    pub state: &'static str,
    /// Grows with every change to a task's activity, also across restarts of
    /// the core: a client keeps the activity with the highest.
    pub seq: u64,
    /// The machine the task's agent session is on, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// That session's status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// False when that machine can't be reached: `status` is the last heard.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reachable: Option<bool>,
}

/// What the board needs of a task.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskRef {
    pub slug: String,
    pub state: String,
    /// The machine it runs on; none is the code machine.
    pub device: Option<String>,
}

/// A running session with an agent, somewhere in the fleet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Live {
    pub device: String,
    pub reachable: bool,
    pub id: String,
    pub agent: String,
    pub status: Status,
}

impl Live {
    /// A session as its core lists it; None unless it runs and has an agent.
    fn of(device: &str, reachable: bool, session: &Value) -> Option<Self> {
        if session["alive"] != true {
            return None;
        }
        Some(Self {
            device: device.to_string(),
            reachable,
            id: session["id"].as_str()?.to_string(),
            agent: session["agent"].as_str()?.to_string(),
            status: serde_json::from_value(session["status"].clone()).ok()?,
        })
    }
}

/// Give a session from a core before statuses the status its three states
/// stand for, as of now. `again`: it has one from an earlier state.
fn stamp(session: &mut Value, again: bool) {
    if session["status"].is_object() && !again {
        return;
    }
    let Ok(state) = serde_json::from_value::<AgentState>(session["agentState"].clone()) else {
        return;
    };
    let detail = session["agentDetail"].as_str().map(str::to_string);
    let view = status::of_agent_state(state, detail);
    let now = crate::workspace::dates::now_millis();
    session["status"] = serde_json::json!(Status::next(view, Source::Core, now, None));
}

/// The session a task's activity goes by: the one named after it, on the
/// machine the task runs on if it's there, else wherever it can be reached.
fn session_of<'a>(task: &TaskRef, sessions: &'a [Live], code_machine: &str) -> Option<&'a Live> {
    let home = task.device.as_deref().unwrap_or(code_machine);
    let named = || sessions.iter().filter(|live| live.id == task.slug);
    named()
        .find(|live| live.device == home)
        .or_else(|| named().find(|live| live.reachable))
        .or_else(|| named().next())
}

/// The rule, first match wins: a completed task is done; then the agent's
/// status decides; without an agent, a task in progress is paused.
pub fn word(task_state: &str, session: Option<State>) -> &'static str {
    if task_state == "completed" {
        return "done";
    }
    match session {
        Some(State::Blocked) => "needs-you",
        Some(State::Failed) => "error",
        Some(State::Working | State::Starting) => "working",
        Some(State::Ready) => "ready",
        // The agent exited; its terminal may still be open
        Some(State::Ended) | None if task_state == "in-progress" => "paused",
        Some(State::Ended) | None => "not-started",
    }
}

/// A task's activity without its `seq`; None for a task that says it all by
/// being completed.
fn decide(task: &TaskRef, sessions: &[Live], code_machine: &str) -> Option<Activity> {
    let session = session_of(task, sessions, code_machine);
    if session.is_none() && task.state == "completed" {
        return None;
    }
    Some(Activity {
        state: word(&task.state, session.map(|live| live.status.state)),
        seq: 0,
        device: session.map(|live| live.device.clone()),
        agent: session.map(|live| live.agent.clone()),
        status: session.map(|live| live.status.clone()),
        reachable: session.map(|live| live.reachable),
    })
}

#[derive(Default)]
struct Machine {
    online: bool,
    sessions: BTreeMap<String, Value>,
}

/// The sessions on the fleet's other machines, as their events tell them.
#[derive(Default)]
pub struct Mirror {
    machines: HashMap<String, Machine>,
}

impl Mirror {
    /// Take in an event from a machine's own stream. True if it changed a session.
    pub fn apply(&mut self, device: &str, event: &Value) -> bool {
        let kind = event["type"].as_str().unwrap_or_default();
        // What a machine sends first, and again when its listener fell behind
        if matches!(kind, "sessions" | "resync") {
            let sessions = event["sessions"].as_array().cloned().unwrap_or_default();
            self.replace(device, sessions);
            return true;
        }
        let machine = self.machines.entry(device.to_string()).or_default();
        let id = event["id"].as_str().unwrap_or_default();
        match kind {
            "sessionCreated" => {
                let mut session = event["session"].clone();
                let Some(id) = session["id"].as_str().map(str::to_string) else {
                    return false;
                };
                stamp(&mut session, false);
                machine.sessions.insert(id, session);
            }
            "sessionRemoved" => return machine.sessions.remove(id).is_some(),
            kind => {
                let Some(session) = machine.sessions.get_mut(id) else {
                    return false;
                };
                match kind {
                    "sessionStatus" => {
                        // The list it came with may already have a newer one
                        if event["status"]["seq"].as_u64() < session["status"]["seq"].as_u64() {
                            return false;
                        }
                        session["status"] = event["status"].clone();
                    }
                    "agentState" => {
                        session["agentState"] = event["state"].clone();
                        session["agentDetail"] = event["detail"].clone();
                        // Without `seq` no status came before it: an older core
                        if event["seq"].is_null() {
                            stamp(session, true);
                        }
                    }
                    "sessionExited" => {
                        session["alive"] = false.into();
                        session["exitCode"] = event["exitCode"].clone();
                    }
                    "sessionResized" => {
                        session["cols"] = event["cols"].clone();
                        session["rows"] = event["rows"].clone();
                    }
                    _ => return false,
                }
            }
        }
        true
    }

    /// A machine's sessions, all of them, as it lists them now.
    pub fn replace(&mut self, device: &str, sessions: Vec<Value>) {
        let machine = self.machines.entry(device.to_string()).or_default();
        machine.sessions = sessions
            .into_iter()
            .filter_map(|mut session| {
                stamp(&mut session, false);
                Some((session["id"].as_str()?.to_string(), session))
            })
            .collect();
    }

    /// True if that's news.
    pub fn set_online(&mut self, device: &str, online: bool) -> bool {
        let machine = self.machines.entry(device.to_string()).or_default();
        std::mem::replace(&mut machine.online, online) != online
    }

    /// Forget the machines that left the fleet.
    pub fn keep(&mut self, devices: &[String]) {
        self.machines.retain(|id, _| devices.contains(id));
    }

    /// The sessions on the machines that can be reached, each with its
    /// `device`.
    pub fn listed(&self) -> Vec<Value> {
        let reachable = self.machines.iter().filter(|(_, machine)| machine.online);
        reachable
            .flat_map(|(device, machine)| {
                machine.sessions.values().map(move |session| {
                    let mut session = session.clone();
                    session["device"] = device.clone().into();
                    session
                })
            })
            .collect()
    }

    fn live(&self) -> Vec<Live> {
        let sessions = self.machines.iter().flat_map(|(device, machine)| {
            let sessions = machine.sessions.values();
            sessions.filter_map(move |session| Live::of(device, machine.online, session))
        });
        sessions.collect()
    }
}

#[derive(Default)]
struct Inner {
    mirror: Mirror,
    tasks: Vec<TaskRef>,
    /// What clients were last told, by task.
    shown: BTreeMap<String, Activity>,
}

/// Every task's activity, kept up to date by `api::activity`.
#[derive(Default)]
pub struct Board {
    inner: Mutex<Inner>,
}

impl Board {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Take in an event. True if it may have changed a task's activity: a
    /// session somewhere started, ended or changed status, or a machine came
    /// or went.
    pub fn see(&self, event: &Event) -> bool {
        match event {
            Event::DeviceEvent { device, event } => self.lock().mirror.apply(device, event),
            Event::DeviceStatus { device, online } => {
                self.lock().mirror.set_online(device, *online)
            }
            Event::SessionCreated { .. }
            | Event::SessionStatus { .. }
            | Event::SessionExited { .. }
            | Event::SessionRemoved { .. } => true,
            _ => false,
        }
    }

    pub fn set_tasks(&self, tasks: Vec<TaskRef>) {
        self.lock().tasks = tasks;
    }

    /// A machine's sessions as it lists them now, after events were missed.
    pub fn replace(&self, device: &str, sessions: Vec<Value>) {
        self.lock().mirror.replace(device, sessions);
    }

    /// The other machines' sessions, for `GET /v0/fleet/sessions`.
    pub fn fleet_sessions(&self) -> Vec<Value> {
        self.lock().mirror.listed()
    }

    /// Every task's activity, by slug; a task that isn't there is done if
    /// it's completed, else not started.
    pub fn all(&self) -> BTreeMap<String, Activity> {
        self.lock().shown.clone()
    }

    /// Work every task's activity out again and return the ones that
    /// changed, for the clients to hear. None: the task has none any more.
    /// `own` are this core's sessions, `devices` the fleet's other machines.
    pub fn update(
        &self,
        own: &[SessionInfo],
        own_id: &str,
        devices: &[String],
        code_machine: &str,
    ) -> Vec<(String, Option<Activity>)> {
        let mut inner = self.lock();
        inner.mirror.keep(devices);
        let mut sessions = inner.mirror.live();
        sessions.extend(own.iter().filter_map(|session| {
            Some(Live {
                device: own_id.to_string(),
                reachable: true,
                id: session.id.clone(),
                agent: session.agent.clone().filter(|_| session.alive)?,
                status: session.status.clone()?,
            })
        }));
        let now = u64::try_from(crate::workspace::dates::now_millis()).unwrap_or(0);
        let mut changed = Vec::new();
        let mut next = BTreeMap::new();
        for task in &inner.tasks {
            let Some(mut activity) = decide(task, &sessions, code_machine) else {
                continue;
            };
            let before = inner.shown.get(&task.slug);
            activity.seq = before.map_or(0, |shown| shown.seq);
            if before != Some(&activity) {
                // The clock keeps the count growing across restarts
                activity.seq = (activity.seq + 1).max(now);
                changed.push((task.slug.clone(), Some(activity.clone())));
            }
            next.insert(task.slug.clone(), activity);
        }
        let gone = inner.shown.keys().filter(|slug| !next.contains_key(*slug));
        changed.extend(gone.map(|slug| (slug.clone(), None)));
        inner.shown = next;
        changed
    }

    /// This core isn't the home: it has no activities to tell.
    pub fn clear(&self) -> Vec<(String, Option<Activity>)> {
        let shown = std::mem::take(&mut self.lock().shown);
        shown.into_keys().map(|slug| (slug, None)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::status::View;
    use serde_json::json;

    fn task(slug: &str, state: &str, device: Option<&str>) -> TaskRef {
        TaskRef {
            slug: slug.into(),
            state: state.into(),
            device: device.map(str::to_string),
        }
    }

    fn status(state: State, seq: u64) -> Value {
        let mut status = Status::next(View::new(state, None, None), Source::Hooks, 0, None);
        status.seq = seq;
        json!(status)
    }

    fn session(id: &str, state: State) -> Value {
        json!({ "id": id, "alive": true, "agent": "claude", "status": status(state, 5) })
    }

    /// A board whose fleet has `cube`, online, with these sessions.
    fn fleet_with(tasks: Vec<TaskRef>, on_cube: Vec<Value>) -> Board {
        let board = Board::default();
        board.set_tasks(tasks);
        board.see(&Event::DeviceStatus {
            device: "cube".into(),
            online: true,
        });
        board.see(&Event::DeviceEvent {
            device: "cube".into(),
            event: json!({ "type": "sessions", "sessions": on_cube }),
        });
        board
    }

    fn update(board: &Board) -> Vec<(String, Option<&'static str>)> {
        let changed = board.update(&[], "mac", &["cube".to_string()], "mac");
        changed
            .into_iter()
            .map(|(slug, activity)| (slug, activity.map(|a| a.state)))
            .collect()
    }

    fn relayed(board: &Board, event: Value) -> bool {
        board.see(&Event::DeviceEvent {
            device: "cube".into(),
            event,
        })
    }

    #[test]
    fn the_rule_should_put_the_task_s_own_state_before_and_after_its_agent() {
        // The shared table of cases, which the apps check their words against
        let cases: Value =
            serde_json::from_str(include_str!("../docs/status-cases.json")).expect("json");
        let cases = cases["activity"].as_array().expect("cases");
        assert!(cases.len() >= 12);
        for case in cases {
            let session: Option<State> =
                serde_json::from_value(case["session"].clone()).expect("a state or null");
            let task = case["task"].as_str().expect("a task state");
            assert_eq!(word(task, session), case["shows"], "{case}");
        }
    }

    #[test]
    fn a_task_should_go_by_its_session_wherever_that_runs() {
        let board = fleet_with(
            vec![
                task("asks", "in-progress", Some("cube")),
                task("idle", "in-progress", None),
                task("new", "not-started", None),
                task("old", "completed", None),
            ],
            vec![session("asks", State::Blocked)],
        );
        assert_eq!(
            update(&board),
            [
                ("asks".to_string(), Some("needs-you")),
                ("idle".to_string(), Some("paused")),
                ("new".to_string(), Some("not-started")),
            ]
        );
        let all = board.all();
        assert_eq!(all["asks"].device.as_deref(), Some("cube"));
        assert_eq!(all["asks"].reachable, Some(true));
        assert_eq!(all["idle"].device, None);
        assert!(!all.contains_key("old"));
        // Nothing changed: nothing to tell
        assert!(update(&board).is_empty());
    }

    #[test]
    fn a_status_event_should_change_the_task_and_raise_its_seq() {
        let board = fleet_with(
            vec![task("t", "in-progress", Some("cube"))],
            vec![session("t", State::Working)],
        );
        update(&board);
        let first = board.all()["t"].seq;

        let event =
            json!({ "type": "sessionStatus", "id": "t", "status": status(State::Blocked, 6) });
        assert!(relayed(&board, event));
        assert_eq!(update(&board), [("t".to_string(), Some("needs-you"))]);
        assert!(board.all()["t"].seq > first);

        // An older status than the one the mirror has changes nothing
        let late =
            json!({ "type": "sessionStatus", "id": "t", "status": status(State::Working, 4) });
        assert!(!relayed(&board, late));
        assert!(update(&board).is_empty());

        let exited = json!({ "type": "sessionExited", "id": "t", "exitCode": 0 });
        assert!(relayed(&board, exited));
        assert_eq!(update(&board), [("t".to_string(), Some("paused"))]);
    }

    #[test]
    fn completing_a_task_should_end_its_activity_once_its_session_is_gone() {
        let board = fleet_with(
            vec![task("t", "in-progress", Some("cube"))],
            vec![session("t", State::Ready)],
        );
        assert_eq!(update(&board), [("t".to_string(), Some("ready"))]);

        // Completed with its agent still open: done, and still there to open
        board.set_tasks(vec![task("t", "completed", Some("cube"))]);
        assert_eq!(update(&board), [("t".to_string(), Some("done"))]);
        assert!(board.all()["t"].status.is_some());

        assert!(relayed(
            &board,
            json!({ "type": "sessionRemoved", "id": "t" })
        ));
        assert_eq!(update(&board), [("t".to_string(), None)]);
        assert!(board.all().is_empty());
    }

    #[test]
    fn a_machine_out_of_reach_should_keep_its_sessions_as_last_heard() {
        let board = fleet_with(
            vec![task("t", "in-progress", Some("cube"))],
            vec![session("t", State::Working)],
        );
        update(&board);
        assert_eq!(board.fleet_sessions().len(), 1);

        assert!(board.see(&Event::DeviceStatus {
            device: "cube".into(),
            online: false,
        }));
        assert_eq!(update(&board), [("t".to_string(), Some("working"))]);
        assert_eq!(board.all()["t"].reachable, Some(false));
        // Listed sessions are the ones that can be opened
        assert!(board.fleet_sessions().is_empty());

        // The machine left the fleet
        let changed = board.update(&[], "mac", &[], "mac");
        assert_eq!(changed[0].1.as_ref().map(|a| a.state), Some("paused"));
    }

    #[test]
    fn a_task_should_prefer_the_session_on_its_own_machine() {
        let board = fleet_with(
            vec![task("t", "in-progress", None)],
            vec![session("t", State::Blocked)],
        );
        // The task runs in place on the code machine (this one), where its
        // session works; cube has an older one of the same name
        let own = SessionInfo {
            id: "t".into(),
            alive: true,
            exit_code: None,
            agent: Some("claude".into()),
            status: serde_json::from_value(status(State::Working, 9)).ok(),
            agent_state: None,
            agent_detail: None,
            pid: None,
            cols: 80,
            rows: 24,
            labels: BTreeMap::new(),
        };
        let changed = board.update(&[own], "mac", &["cube".to_string()], "mac");
        let activity = changed[0].1.as_ref().expect("an activity");
        assert_eq!(activity.state, "working");
        assert_eq!(activity.device.as_deref(), Some("mac"));
    }

    #[test]
    fn a_session_from_an_older_core_should_count_by_its_three_states() {
        let older = json!({ "id": "t", "alive": true, "agent": "claude", "agentState": "waiting", "agentDetail": "Go?" });
        let board = fleet_with(vec![task("t", "in-progress", Some("cube"))], vec![older]);
        assert_eq!(update(&board), [("t".to_string(), Some("ready"))]);
        let status = board.all()["t"].status.clone().expect("a status");
        assert_eq!(status.detail.as_deref(), Some("Go?"));

        let working = json!({ "type": "agentState", "id": "t", "state": "working" });
        assert!(relayed(&board, working));
        assert_eq!(update(&board), [("t".to_string(), Some("working"))]);

        // A plain terminal isn't an agent
        let terminal = json!({ "id": "t", "alive": true, "agent": null, "agentState": null });
        let plain = fleet_with(vec![task("t", "not-started", None)], vec![terminal]);
        assert_eq!(update(&plain), [("t".to_string(), Some("not-started"))]);
        assert_eq!(plain.all()["t"].device, None);
    }
}
