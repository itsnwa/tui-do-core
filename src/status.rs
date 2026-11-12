//! What an agent session is doing, as one status that every client shows the
//! same way.
//!
//! The status comes from the agent's hooks: each hook appends a [`Record`] to
//! the session's log, and [`Agent`] turns the records into a status. Claude
//! has no hook for an interrupted turn or for a permission being answered,
//! so its own account of itself (see `claude_status`) corrects the status
//! where the hooks stay silent ([`correction`]).

use serde::{Deserialize, Serialize};

use crate::session::AgentState;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// The session is up; its agent hasn't reported yet.
    Starting,
    /// The agent is in a turn.
    Working,
    /// It can't go on without an answer: a permission, a question, a plan.
    Blocked,
    /// Its turn is over; the next move is the user's.
    Ready,
    /// Its turn ended on an error.
    Failed,
    /// The agent exited, or the session did.
    Ended,
}

/// Where a status comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The agent's hooks reported it.
    Hooks,
    /// Claude's own status said so where the hooks couldn't.
    Claude,
    /// Guessed from the session's output.
    Inferred,
    /// The core knows: the session ended.
    Core,
}

/// A status as sessions carry it and events send it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub state: State,
    /// Why, in a word (see `docs/api.md` for the words of each state).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// One line for a person: the question, the tool it wants, the last
    /// line of its reply, the error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// When it took this status (ISO 8601).
    pub since: String,
    /// Grows with every change to a session's status, also across restarts
    /// of the core: a client keeps the status with the highest.
    pub seq: u64,
    pub source: Source,
}

/// What a status says, without when or how it's known.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    pub state: State,
    pub reason: Option<String>,
    pub detail: Option<String>,
}

impl View {
    pub fn new(state: State, reason: Option<&str>, detail: Option<String>) -> Self {
        Self {
            state,
            reason: reason.map(str::to_string),
            detail,
        }
    }

    /// The state for clients from before statuses, which only know three.
    pub fn agent_state(&self) -> AgentState {
        match self.state {
            State::Starting | State::Ended => AgentState::Idle,
            State::Working => AgentState::Working,
            State::Blocked | State::Failed => AgentState::Waiting,
            // Nothing to answer: it just started, or the user stopped it
            State::Ready => match self.reason.as_deref() {
                Some("started" | "idle" | "interrupted") => AgentState::Idle,
                _ => AgentState::Waiting,
            },
        }
    }

    /// Whether someone should be told: it needs an answer, it failed, or it
    /// finished. Not when the user stopped it, or it will go on by itself.
    pub fn calls_for_attention(&self) -> bool {
        match self.state {
            State::Blocked | State::Failed => true,
            State::Ready => matches!(self.reason.as_deref(), Some("finished" | "quiet")),
            State::Starting | State::Working | State::Ended => false,
        }
    }
}

impl Status {
    /// A session's next status. `at` is when it took it (milliseconds since
    /// the epoch), `after` the status it had.
    pub fn next(view: View, source: Source, at: i64, after: Option<&Status>) -> Self {
        // The clock keeps the count growing across restarts of the core
        let now = u64::try_from(crate::workspace::dates::now_millis()).unwrap_or(0);
        Self {
            state: view.state,
            reason: view.reason,
            detail: view.detail,
            since: crate::format::js::iso_string(at),
            seq: after.map_or(0, |status| status.seq + 1).max(now),
            source,
        }
    }

    pub fn view(&self) -> View {
        View {
            state: self.state,
            reason: self.reason.clone(),
            detail: self.detail.clone(),
        }
    }
}

/// The status a state from before statuses stands for (an older core's
/// session, as the home sees it).
pub fn of_agent_state(state: AgentState, detail: Option<String>) -> View {
    match state {
        AgentState::Idle => View::new(State::Ready, Some("idle"), None),
        AgentState::Working => View::new(State::Working, Some("turn"), None),
        AgentState::Waiting => View::new(State::Ready, Some("finished"), detail),
    }
}

/// The status behind a state word and the hook input saved with it: how
/// Claude sessions started by cores before 0.3.18 report, and Codex.
pub fn of_state_word(state: AgentState, input: Option<&serde_json::Value>) -> View {
    let AgentState::Waiting = state else {
        return of_agent_state(state, None);
    };
    let null = serde_json::Value::Null;
    let input = input.unwrap_or(&null);
    let detail = crate::hooks::detail(input);
    let kind = input["notification_type"].as_str().unwrap_or_default();
    match input["hook_event_name"].as_str().unwrap_or_default() {
        "PermissionRequest" | "PreToolUse" | "Elicitation" => {
            View::new(State::Blocked, Some(ask_reason(input)), detail)
        }
        "Notification" if kind.contains("permission") => {
            View::new(State::Blocked, Some("permission"), detail)
        }
        "Notification" if kind != "idle_prompt" => {
            View::new(State::Blocked, Some("question"), detail)
        }
        "StopFailure" => View::new(
            State::Failed,
            Some(input["error"].as_str().unwrap_or("unknown")),
            detail,
        ),
        _ => View::new(State::Ready, Some("finished"), detail),
    }
}

/// What a hook's input asks the user for.
fn ask_reason(input: &serde_json::Value) -> &'static str {
    match (
        input["hook_event_name"].as_str(),
        input["tool_name"].as_str(),
    ) {
        (_, Some("AskUserQuestion")) | (Some("Elicitation"), _) => "question",
        (_, Some("ExitPlanMode")) => "plan",
        _ => "permission",
    }
}

/// One hook event, as a line of a session's log. Written by `tui-do-core
/// hook` from what the agent hands its hooks, so only fields may be added.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// When, in milliseconds since the epoch.
    pub t: i64,
    /// The hook's name, or one of the `CLAUDE_*` events.
    pub event: String,
    /// The agent's own session id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    /// The tool call's id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// A hash of the tool's input: a permission request names its tool call
    /// only by tool and input.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<u64>,
    /// The subagent the event comes from; none for the session's own agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<String>,
    /// The event's own kind: a notification's type, what started or ended
    /// the session, the error a turn ended on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// The turn ended with agents of its own still running, which bring it back.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub background: bool,
}

/// Claude's own status says it's working again.
pub const CLAUDE_BUSY: &str = "Claude:busy";
/// Claude's own status says its turn is over.
pub const CLAUDE_IDLE: &str = "Claude:idle";
/// Claude's own status says it waits for the user.
pub const CLAUDE_WAITING: &str = "Claude:waiting";
/// Claude's process is gone.
pub const CLAUDE_GONE: &str = "Claude:gone";

/// FNV-1a: the same on every machine and release, unlike std's hasher.
fn hash(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Record {
    /// The record of a hook's input (what Claude Code and Codex pipe to a
    /// hook's command); None if it isn't one.
    pub fn of_hook(input: &serde_json::Value, now: i64) -> Option<Self> {
        let event = input["hook_event_name"].as_str()?;
        let text = |key: &str| input[key].as_str().map(str::to_string);
        let kind = match event {
            "Notification" => text("notification_type"),
            "SessionStart" => text("source"),
            "SessionEnd" => text("reason"),
            "StopFailure" => text("error"),
            _ => None,
        };
        let asks = matches!(
            input["tool_name"].as_str(),
            Some("AskUserQuestion" | "ExitPlanMode")
        );
        let says = match event {
            "PermissionRequest" | "Elicitation" | "Notification" | "Stop" | "StopFailure" => true,
            "PreToolUse" => asks,
            _ => false,
        };
        let background = event == "Stop"
            && input["background_tasks"].as_array().is_some_and(|tasks| {
                tasks.iter().any(|task| {
                    matches!(
                        task["type"].as_str(),
                        Some("subagent" | "workflow" | "teammate")
                    )
                })
            });
        Some(Self {
            t: now,
            event: event.to_string(),
            session: text("session_id"),
            tool: text("tool_name"),
            id: text("tool_use_id"),
            input: input
                .get("tool_input")
                .map(|tool_input| hash(&tool_input.to_string())),
            agent: text("agent_id"),
            kind,
            detail: says.then(|| crate::hooks::detail(input)).flatten(),
            background,
        })
    }

    fn kind_is(&self, kinds: &[&str]) -> bool {
        self.kind
            .as_deref()
            .is_some_and(|kind| kinds.contains(&kind))
    }
}

/// What the agent asks the user, which holds it up until it's answered.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Ask {
    reason: &'static str,
    detail: Option<String>,
    /// When it asked, in milliseconds since the epoch.
    at: i64,
}

/// A tool call that hasn't finished.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tool {
    id: Option<String>,
    /// The subagent that made it; none for the session's own agent.
    agent: Option<String>,
    name: String,
    input: Option<u64>,
    ask: Option<Ask>,
    /// Asked without a tool call to hang it on (a notification, Claude's own
    /// status): nothing says when it's answered, so the next hook ends it.
    loose: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Base {
    Starting,
    Working,
    Ready {
        reason: &'static str,
        detail: Option<String>,
    },
    Failed {
        error: String,
        detail: Option<String>,
    },
    Ended,
}

/// An agent session's status, worked out from its hook records in order.
///
/// One state word, last writer wins, gets parallel tool calls wrong: one
/// finishing would hide another's open permission prompt. So the tool calls
/// under way are kept, each with what it asks the user, and the agent is
/// blocked for as long as any of them asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Agent {
    base: Base,
    tools: Vec<Tool>,
    /// When the last record was made, in milliseconds since the epoch.
    pub at: i64,
    /// The agent's own session id, once a hook said it.
    pub session: Option<String>,
    /// The last record came from Claude's own status, not from a hook.
    pub corrected: bool,
}

impl Default for Agent {
    fn default() -> Self {
        Self {
            base: Base::Starting,
            tools: Vec::new(),
            at: 0,
            session: None,
            corrected: false,
        }
    }
}

impl Agent {
    pub fn apply(&mut self, record: &Record) {
        self.at = record.t;
        self.corrected = record.event.starts_with("Claude:");
        if let Some(session) = &record.session {
            self.session = Some(session.clone());
        }
        let own = record.agent.is_none();
        let detail = || record.detail.clone();
        if record.event != "Notification" && record.event != CLAUDE_WAITING {
            self.tools.retain(|tool| !tool.loose);
        }
        match record.event.as_str() {
            // Compacting happens in the middle of a turn
            "SessionStart" if !record.kind_is(&["compact"]) => {
                self.tools.clear();
                self.base = Base::Ready {
                    reason: "started",
                    detail: None,
                };
            }
            "UserPromptSubmit" => {
                self.tools.clear();
                self.base = Base::Working;
            }
            "PreToolUse" => {
                let ask = match record.tool.as_deref() {
                    Some("AskUserQuestion") => Some("question"),
                    Some("ExitPlanMode") => Some("plan"),
                    _ => None,
                };
                self.tools.push(Tool {
                    id: record.id.clone(),
                    agent: record.agent.clone(),
                    name: record.tool.clone().unwrap_or_default(),
                    input: record.input,
                    ask: ask.map(|reason| Ask {
                        reason,
                        detail: detail(),
                        at: record.t,
                    }),
                    loose: false,
                });
                if own {
                    self.wake();
                }
            }
            "PermissionRequest" => self.ask_permission(record),
            "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
                self.finish_tool(record);
                if own {
                    self.wake();
                }
            }
            // The model is asked again (Claude says the batch is done; Codex
            // that it asks): every tool call of that agent's last batch is
            // over, also one whose permission prompt was refused
            "PostToolBatch" | "PreModelRequest" => {
                self.tools.retain(|tool| tool.agent != record.agent);
            }
            // Codex says when the user stops a turn; Claude doesn't
            "Interrupt" => {
                self.tools.clear();
                self.base = interrupted();
            }
            "Elicitation" => self.tools.push(Tool {
                id: Some("elicitation".into()),
                agent: record.agent.clone(),
                name: "elicitation".into(),
                input: None,
                ask: Some(Ask {
                    reason: "question",
                    detail: detail(),
                    at: record.t,
                }),
                loose: false,
            }),
            "ElicitationResult" => self
                .tools
                .retain(|tool| tool.id.as_deref() != Some("elicitation")),
            "Notification" => {
                let reason = if record.kind_is(&["permission_prompt", "worker_permission_prompt"]) {
                    Some("permission")
                } else if record.kind_is(&[
                    "elicitation_dialog",
                    "elicitation_url_dialog",
                    "agent_needs_input",
                ]) {
                    Some("question")
                } else {
                    None
                };
                if let Some(reason) = reason {
                    self.ask_loosely(reason, record);
                }
            }
            "Stop" => {
                self.tools.clear();
                self.base = Base::Ready {
                    reason: if record.background {
                        "background"
                    } else {
                        "finished"
                    },
                    detail: detail(),
                };
            }
            "StopFailure" => {
                self.tools.clear();
                self.base = Base::Failed {
                    error: record.kind.clone().unwrap_or_else(|| "unknown".into()),
                    detail: detail(),
                };
            }
            // After /clear or /resume the next session starts at once
            "SessionEnd" if !record.kind_is(&["clear", "resume"]) => {
                self.tools.clear();
                self.base = Base::Ended;
            }
            CLAUDE_BUSY => {
                // Whatever it asked has been answered
                for tool in &mut self.tools {
                    tool.ask = None;
                }
                self.wake();
            }
            CLAUDE_IDLE => {
                let asked = self.tools.iter().any(|tool| tool.ask.is_some());
                self.tools.clear();
                match self.base {
                    Base::Starting => {
                        self.base = Base::Ready {
                            reason: "started",
                            detail: None,
                        };
                    }
                    // Its turn is over and no hook said so: the user stopped it
                    Base::Working => self.base = interrupted(),
                    Base::Ready { .. } | Base::Failed { .. } if asked => self.base = interrupted(),
                    _ => {}
                }
            }
            CLAUDE_WAITING => {
                let reason = if record.kind_is(&["permission"]) {
                    "permission"
                } else {
                    "question"
                };
                self.ask_loosely(reason, record);
            }
            CLAUDE_GONE => {
                self.tools.clear();
                self.base = Base::Ended;
            }
            _ => {}
        }
    }

    /// The agent is doing something again, whatever it said last.
    fn wake(&mut self) {
        self.base = Base::Working;
    }

    fn asks(&self) -> bool {
        self.tools.iter().any(|tool| tool.ask.is_some())
    }

    /// When it last asked the user something that's still open.
    fn asked_at(&self) -> Option<i64> {
        let asks = self.tools.iter().filter_map(|tool| tool.ask.as_ref());
        asks.map(|ask| ask.at).max()
    }

    fn ask_loosely(&mut self, reason: &'static str, record: &Record) {
        if !self.asks() {
            self.tools.push(Tool {
                id: None,
                agent: record.agent.clone(),
                name: String::new(),
                input: None,
                ask: Some(Ask {
                    reason,
                    detail: record.detail.clone(),
                    at: record.t,
                }),
                loose: true,
            });
        }
    }

    /// A permission request names its tool call by tool and input.
    fn ask_permission(&mut self, record: &Record) {
        let name = record.tool.clone().unwrap_or_default();
        let same = |tool: &Tool| !tool.loose && tool.name == name && tool.input == record.input;
        // A question or a plan asks through its own tool call already
        if self
            .tools
            .iter()
            .any(|tool| same(tool) && tool.ask.is_some())
        {
            return;
        }
        let ask = Some(Ask {
            reason: "permission",
            detail: record.detail.clone(),
            at: record.t,
        });
        match self.tools.iter_mut().rev().find(|tool| same(tool)) {
            Some(tool) => tool.ask = ask,
            None => self.tools.push(Tool {
                id: None,
                agent: record.agent.clone(),
                name,
                input: record.input,
                ask,
                loose: false,
            }),
        }
    }

    fn finish_tool(&mut self, record: &Record) {
        let by_id = record.id.as_ref().and_then(|id| {
            self.tools
                .iter()
                .position(|tool| tool.id.as_ref() == Some(id))
        });
        // A permission request without its tool call has no id to go by
        let found = by_id.or_else(|| {
            self.tools.iter().position(|tool| {
                tool.id.is_none() && !tool.loose && Some(&tool.name) == record.tool.as_ref()
            })
        });
        if let Some(at) = found {
            self.tools.remove(at);
        }
    }

    pub fn view(&self) -> View {
        if self.base == Base::Ended {
            return View::new(State::Ended, None, None);
        }
        if let Some(ask) = self.tools.iter().find_map(|tool| tool.ask.as_ref()) {
            return View::new(State::Blocked, Some(ask.reason), ask.detail.clone());
        }
        match &self.base {
            Base::Starting => View::new(State::Starting, None, None),
            Base::Working => View::new(State::Working, Some("turn"), None),
            Base::Ready { reason, detail } => View::new(State::Ready, Some(reason), detail.clone()),
            Base::Failed { error, detail } => View::new(State::Failed, Some(error), detail.clone()),
            Base::Ended => View::new(State::Ended, None, None),
        }
    }

    pub fn source(&self) -> Source {
        if self.corrected {
            Source::Claude
        } else {
            Source::Hooks
        }
    }
}

fn interrupted() -> Base {
    Base::Ready {
        reason: "interrupted",
        detail: None,
    }
}

/// What Claude's own status says it's doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Says {
    Busy,
    Idle,
    Waiting,
}

/// Claude's own account of a session (see `claude_status`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaudeStatus {
    pub says: Says,
    /// What it waits for, in Claude's words ("permission prompt").
    pub waiting_for: Option<String>,
    /// Since when, in milliseconds since the epoch.
    pub at: i64,
}

/// How long the hooks and Claude's own status have to disagree before
/// Claude's wins. They change within moments of each other, in either order.
pub const SETTLE_MS: i64 = 1000;

/// The record that brings the hooks' status in line with Claude's own, once
/// the two have disagreed for [`SETTLE_MS`]. None while they agree, where
/// the hooks know better (a finished turn, an error), or when the hooks
/// spoke last: Claude's status only corrects what it changed its mind about
/// afterwards. (While a background agent asks for a permission, say, the
/// session's own agent stays idle.)
pub fn correction(agent: &Agent, claude: &ClaudeStatus, now: i64) -> Option<Record> {
    // An answer follows its question, whatever other tools reported meanwhile
    let hooks_spoke = agent.asked_at().unwrap_or(agent.at);
    if claude.at <= hooks_spoke || now - agent.at.max(claude.at) < SETTLE_MS {
        return None;
    }
    let event = match (agent.view().state, claude.says) {
        (State::Starting | State::Blocked | State::Ready | State::Failed, Says::Busy) => {
            CLAUDE_BUSY
        }
        (State::Starting | State::Working | State::Blocked, Says::Idle) => CLAUDE_IDLE,
        (State::Starting | State::Working, Says::Waiting) => CLAUDE_WAITING,
        _ => return None,
    };
    let waiting = event == CLAUDE_WAITING;
    let waiting_for = claude.waiting_for.as_deref().filter(|_| waiting);
    Some(Record {
        t: now,
        event: event.to_string(),
        kind: waiting_for
            .filter(|text| text.contains("permission"))
            .map(|_| "permission".to_string()),
        detail: waiting_for.map(sentence),
        ..Record::default()
    })
}

/// Claude's "permission prompt", as a line for a person: "Permission prompt".
fn sentence(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// An agent after these hook inputs, a second apart.
    fn after(inputs: &[serde_json::Value]) -> Agent {
        let mut agent = Agent::default();
        for (i, input) in inputs.iter().enumerate() {
            let record = Record::of_hook(input, 1000 * (i as i64 + 1)).expect("a hook input");
            agent.apply(&record);
        }
        agent
    }

    fn hook(event: &str) -> serde_json::Value {
        json!({ "hook_event_name": event, "session_id": "s1" })
    }

    fn tool(event: &str, name: &str, id: &str, input: serde_json::Value) -> serde_json::Value {
        json!({ "hook_event_name": event, "tool_name": name, "tool_use_id": id, "tool_input": input })
    }

    /// A permission request has no tool call id.
    fn permission(name: &str, input: serde_json::Value) -> serde_json::Value {
        json!({ "hook_event_name": "PermissionRequest", "tool_name": name, "tool_input": input })
    }

    fn stop(reply: &str) -> serde_json::Value {
        json!({ "hook_event_name": "Stop", "last_assistant_message": reply, "background_tasks": [] })
    }

    fn view(state: State, reason: &str, detail: Option<&str>) -> View {
        View::new(state, Some(reason), detail.map(str::to_string))
    }

    fn bash(command: &str) -> serde_json::Value {
        json!({ "command": command })
    }

    #[test]
    fn a_turn_should_go_from_started_to_working_to_finished() {
        let started = after(&[hook("SessionStart")]);
        assert_eq!(started.view(), view(State::Ready, "started", None));
        assert_eq!(started.session.as_deref(), Some("s1"));

        let working = after(&[hook("SessionStart"), hook("UserPromptSubmit")]);
        assert_eq!(working.view(), view(State::Working, "turn", None));

        let finished = after(&[
            hook("SessionStart"),
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("ls")),
            tool("PostToolUse", "Bash", "t1", bash("ls")),
            stop("All done.\n\nShould I push it?"),
        ]);
        assert_eq!(
            finished.view(),
            view(State::Ready, "finished", Some("Should I push it?"))
        );
        assert_eq!(finished.source(), Source::Hooks);
    }

    #[test]
    fn a_permission_prompt_should_block_until_its_tool_is_done() {
        let asked = [
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("cargo test")),
            permission("Bash", bash("cargo test")),
        ];
        assert_eq!(
            after(&asked).view(),
            view(
                State::Blocked,
                "permission",
                Some("Allow Bash: cargo test?")
            )
        );

        let mut done = asked.to_vec();
        done.push(tool("PostToolUse", "Bash", "t1", bash("cargo test")));
        assert_eq!(after(&done).view(), view(State::Working, "turn", None));
    }

    #[test]
    fn another_tool_finishing_should_not_hide_an_open_permission_prompt() {
        let agent = after(&[
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Read", "t1", json!({ "file_path": "/a" })),
            tool("PreToolUse", "Bash", "t2", bash("rm -rf build")),
            permission("Bash", bash("rm -rf build")),
            tool("PostToolUse", "Read", "t1", json!({ "file_path": "/a" })),
        ]);
        assert_eq!(
            agent.view(),
            view(
                State::Blocked,
                "permission",
                Some("Allow Bash: rm -rf build?")
            )
        );
    }

    #[test]
    fn the_same_tool_twice_should_ask_for_the_call_with_that_input() {
        let agent = after(&[
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("ls")),
            tool("PreToolUse", "Bash", "t2", bash("rm x")),
            permission("Bash", bash("rm x")),
            tool("PostToolUse", "Bash", "t2", bash("rm x")),
        ]);
        // The call that asked is done; the other never asked
        assert_eq!(agent.view(), view(State::Working, "turn", None));
    }

    #[test]
    fn a_denied_permission_should_let_the_agent_go_on() {
        let agent = after(&[
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("rm x")),
            permission("Bash", bash("rm x")),
            tool("PermissionDenied", "Bash", "t1", bash("rm x")),
        ]);
        assert_eq!(agent.view(), view(State::Working, "turn", None));
    }

    #[test]
    fn a_refused_permission_should_end_with_its_batch() {
        let asked = [
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("rm x")),
            permission("Bash", bash("rm x")),
        ];
        // No hook says the user refused; the batch being over does
        for over in ["PostToolBatch", "PreModelRequest"] {
            let mut done = asked.to_vec();
            done.push(hook(over));
            assert_eq!(
                after(&done).view(),
                view(State::Working, "turn", None),
                "{over}"
            );
        }

        // A background agent's batch isn't the session's own
        let mut theirs = asked.to_vec();
        let mut batch = hook("PostToolBatch");
        batch["agent_id"] = json!("agent-1");
        theirs.push(batch);
        assert_eq!(after(&theirs).view().state, State::Blocked);

        // A model request after the turn (a summary, say) isn't work
        let finished = after(&[
            hook("UserPromptSubmit"),
            stop("Done."),
            hook("PreModelRequest"),
        ]);
        assert_eq!(finished.view().state, State::Ready);
    }

    #[test]
    fn codex_should_say_when_a_turn_is_interrupted() {
        let agent = after(&[
            hook("UserPromptSubmit"),
            tool("PreToolUse", "shell", "t1", bash("make")),
            permission("shell", bash("make")),
            hook("Interrupt"),
        ]);
        assert_eq!(agent.view(), view(State::Ready, "interrupted", None));
        assert!(!agent.view().calls_for_attention());
        assert_eq!(agent.source(), Source::Hooks);
    }

    #[test]
    fn a_question_and_a_plan_should_block_as_what_they_are() {
        let question = json!({ "questions": [{ "question": "Which one?" }] });
        let asked = [
            hook("UserPromptSubmit"),
            tool("PreToolUse", "AskUserQuestion", "t1", question.clone()),
            // Claude may also ask permission for it: still a question
            permission("AskUserQuestion", question.clone()),
        ];
        assert_eq!(
            after(&asked).view(),
            view(State::Blocked, "question", Some("Which one?"))
        );
        let mut answered = asked.to_vec();
        answered.push(tool("PostToolUse", "AskUserQuestion", "t1", question));
        assert_eq!(after(&answered).view(), view(State::Working, "turn", None));

        let plan = after(&[
            hook("UserPromptSubmit"),
            tool(
                "PreToolUse",
                "ExitPlanMode",
                "t1",
                json!({ "plan": "# Plan" }),
            ),
        ]);
        assert_eq!(
            plan.view(),
            view(State::Blocked, "plan", Some("Plan ready for review"))
        );
    }

    #[test]
    fn a_permission_request_without_its_tool_call_should_still_block_and_end() {
        let asked = [hook("UserPromptSubmit"), permission("Bash", bash("make"))];
        assert_eq!(after(&asked).view().state, State::Blocked);
        let mut done = asked.to_vec();
        done.push(tool("PostToolUse", "Bash", "t9", bash("make")));
        assert_eq!(after(&done).view().state, State::Working);
    }

    #[test]
    fn a_notification_should_only_block_until_the_next_hook() {
        let prompt = json!({ "hook_event_name": "Notification", "notification_type": "permission_prompt", "message": "Claude needs your permission to use the network" });
        let asked = [hook("UserPromptSubmit"), prompt];
        assert_eq!(
            after(&asked).view(),
            view(
                State::Blocked,
                "permission",
                Some("Claude needs your permission to use the network")
            )
        );
        let mut on = asked.to_vec();
        on.push(tool("PostToolUse", "Bash", "t1", bash("curl x")));
        assert_eq!(after(&on).view().state, State::Working);

        // An idle prompt isn't a question
        let idle = json!({ "hook_event_name": "Notification", "notification_type": "idle_prompt", "message": "Claude is waiting for your input" });
        let agent = after(&[hook("UserPromptSubmit"), stop("Done."), idle]);
        assert_eq!(agent.view(), view(State::Ready, "finished", Some("Done.")));
    }

    #[test]
    fn a_turn_that_ends_on_an_error_should_be_failed() {
        let failed = json!({ "hook_event_name": "StopFailure", "error": "rate_limit", "last_assistant_message": "API Error: Rate limit reached" });
        let agent = after(&[hook("UserPromptSubmit"), failed]);
        assert_eq!(
            agent.view(),
            view(
                State::Failed,
                "rate_limit",
                Some("API Error: Rate limit reached")
            )
        );
        assert!(agent.view().calls_for_attention());
    }

    #[test]
    fn a_turn_that_leaves_agents_running_should_say_so_and_wake_by_itself() {
        let waiting = json!({ "hook_event_name": "Stop", "last_assistant_message": "Three agents are on it.", "background_tasks": [{ "id": "a", "type": "subagent", "status": "running" }] });
        let left = [hook("UserPromptSubmit"), waiting];
        let agent = after(&left);
        assert_eq!(
            agent.view(),
            view(State::Ready, "background", Some("Three agents are on it."))
        );
        assert!(!agent.view().calls_for_attention());

        // A background agent's own tools don't mean the session's agent is back
        let mut theirs = left.to_vec();
        let mut call = tool("PreToolUse", "Bash", "t1", bash("ls"));
        call["agent_id"] = json!("agent-1");
        theirs.push(call);
        assert_eq!(after(&theirs).view().state, State::Ready);

        // Its own are: nobody submitted a prompt, yet it works
        let mut own = left.to_vec();
        own.push(tool("PreToolUse", "Bash", "t2", bash("ls")));
        assert_eq!(after(&own).view().state, State::Working);

        // A dev server left running isn't the agent's own work
        let server = json!({ "hook_event_name": "Stop", "last_assistant_message": "Done.", "background_tasks": [{ "id": "s", "type": "shell", "status": "running" }] });
        assert_eq!(
            after(&[server]).view(),
            view(State::Ready, "finished", Some("Done."))
        );
    }

    #[test]
    fn a_session_should_end_when_claude_exits_but_not_on_clear() {
        let end = |reason: &str| json!({ "hook_event_name": "SessionEnd", "reason": reason });
        let exited = after(&[hook("SessionStart"), end("prompt_input_exit")]);
        assert_eq!(exited.view(), View::new(State::Ended, None, None));
        let cleared = after(&[hook("UserPromptSubmit"), stop("Done."), end("clear")]);
        assert_eq!(cleared.view().state, State::Ready);
        // Compacting doesn't start a turn over
        let compact = json!({ "hook_event_name": "SessionStart", "source": "compact" });
        let compacted = after(&[hook("UserPromptSubmit"), compact]);
        assert_eq!(compacted.view().state, State::Working);
    }

    fn claude(says: Says, waiting_for: Option<&str>, at: i64) -> ClaudeStatus {
        ClaudeStatus {
            says,
            waiting_for: waiting_for.map(str::to_string),
            at,
        }
    }

    /// The agent after Claude's own status has had time to correct it.
    fn corrected(mut agent: Agent, says: Says, waiting_for: Option<&str>) -> Agent {
        let now = agent.at + 10 * SETTLE_MS;
        let record = correction(&agent, &claude(says, waiting_for, agent.at + 1), now)
            .expect("a correction");
        agent.apply(&record);
        agent
    }

    #[test]
    fn claude_being_busy_should_end_an_answered_permission_prompt() {
        let asked = after(&[
            hook("UserPromptSubmit"),
            tool("PreToolUse", "Bash", "t1", bash("cargo build")),
            permission("Bash", bash("cargo build")),
        ]);
        let running = corrected(asked, Says::Busy, None);
        assert_eq!(running.view(), view(State::Working, "turn", None));
        assert_eq!(running.source(), Source::Claude);

        // The tool finishing is a hook's word again
        let mut done = running;
        let post = tool("PostToolUse", "Bash", "t1", bash("cargo build"));
        done.apply(&Record::of_hook(&post, done.at + 5).expect("a hook input"));
        assert_eq!(done.view(), view(State::Working, "turn", None));
        assert_eq!(done.source(), Source::Hooks);
    }

    #[test]
    fn claude_being_idle_should_end_a_turn_no_hook_ended() {
        let working = after(&[hook("UserPromptSubmit")]);
        assert_eq!(
            corrected(working, Says::Idle, None).view(),
            view(State::Ready, "interrupted", None)
        );
        let asked = after(&[hook("UserPromptSubmit"), permission("Bash", bash("make"))]);
        let stopped = corrected(asked, Says::Idle, None);
        assert_eq!(stopped.view(), view(State::Ready, "interrupted", None));
        assert!(!stopped.view().calls_for_attention());
    }

    #[test]
    fn claude_waiting_should_block_where_no_hook_said_so() {
        let working = after(&[hook("UserPromptSubmit")]);
        let blocked = corrected(working, Says::Waiting, Some("permission prompt"));
        assert_eq!(
            blocked.view(),
            view(State::Blocked, "permission", Some("Permission prompt"))
        );
    }

    #[test]
    fn claude_being_busy_should_wake_a_finished_turn() {
        let finished = after(&[hook("UserPromptSubmit"), stop("Done.")]);
        assert_eq!(
            corrected(finished, Says::Busy, None).view(),
            view(State::Working, "turn", None)
        );
    }

    #[test]
    fn nothing_should_be_corrected_while_the_two_agree_or_only_just_differ() {
        let working = after(&[hook("UserPromptSubmit")]);
        let at = working.at;
        // Claude went idle a moment before its Stop hook gets to say why
        assert_eq!(
            correction(&working, &claude(Says::Idle, None, at + 10), at + 500),
            None
        );
        assert_eq!(
            correction(&working, &claude(Says::Busy, None, at), at + 60_000),
            None
        );
        // The hooks know better how a turn ended
        let finished = after(&[hook("UserPromptSubmit"), stop("Done.")]);
        let idle = claude(Says::Idle, None, finished.at);
        assert_eq!(correction(&finished, &idle, finished.at + 60_000), None);
        let waits = claude(Says::Waiting, Some("input needed"), finished.at);
        assert_eq!(correction(&finished, &waits, finished.at + 60_000), None);
        // Claude was busy all along, and then a hook said it asks: the hook's right
        let asked = after(&[hook("UserPromptSubmit"), permission("Bash", bash("make"))]);
        let busy = claude(Says::Busy, None, 1000);
        assert_eq!(correction(&asked, &busy, asked.at + 60_000), None);
        let ended = after(&[json!({ "hook_event_name": "SessionEnd", "reason": "other" })]);
        let busy = claude(Says::Busy, None, ended.at);
        assert_eq!(correction(&ended, &busy, ended.at + 60_000), None);
    }

    #[test]
    fn a_state_word_should_read_as_the_status_its_hook_meant() {
        let asked = permission("Bash", bash("ls"));
        assert_eq!(
            of_state_word(AgentState::Waiting, Some(&asked)),
            view(State::Blocked, "permission", Some("Allow Bash: ls?"))
        );
        assert_eq!(
            of_state_word(AgentState::Waiting, Some(&stop("Done."))),
            view(State::Ready, "finished", Some("Done."))
        );
        let codex = json!({ "last-assistant-message": "All set" });
        assert_eq!(
            of_state_word(AgentState::Waiting, Some(&codex)),
            view(State::Ready, "finished", Some("All set"))
        );
        assert_eq!(
            of_state_word(AgentState::Waiting, None),
            view(State::Ready, "finished", None)
        );
        assert_eq!(
            of_state_word(AgentState::Working, None),
            view(State::Working, "turn", None)
        );
    }

    #[test]
    fn older_clients_should_get_one_of_their_three_states() {
        let state = |state, reason: &str| view(state, reason, None).agent_state();
        assert_eq!(state(State::Working, "turn"), AgentState::Working);
        assert_eq!(state(State::Blocked, "permission"), AgentState::Waiting);
        assert_eq!(state(State::Failed, "rate_limit"), AgentState::Waiting);
        assert_eq!(state(State::Ready, "finished"), AgentState::Waiting);
        assert_eq!(state(State::Ready, "started"), AgentState::Idle);
        assert_eq!(state(State::Ready, "interrupted"), AgentState::Idle);
        assert_eq!(
            View::new(State::Ended, None, None).agent_state(),
            AgentState::Idle
        );
    }

    #[test]
    fn a_record_should_survive_its_log_line() {
        let asked = permission("Bash", bash("ls"));
        let record = Record::of_hook(&asked, 42).expect("a hook input");
        let line = serde_json::to_string(&record).expect("json");
        assert!(!line.contains('\n'));
        assert_eq!(
            serde_json::from_str::<Record>(&line).expect("a record"),
            record
        );
        assert_eq!(Record::of_hook(&json!({ "message": "no event" }), 1), None);
    }
}
