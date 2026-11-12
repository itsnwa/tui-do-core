//! Terminal sessions.
//!
//! Each session's program runs in a holder process (see `holder`), which
//! outlives the core, so sessions keep running through an update or a crash.
//! The core mirrors the output into a headless libghostty terminal, so it
//! always knows the current screen. A client attaching from anywhere gets that
//! screen as a VT snapshot, then live output.
//!
//! libghostty's `Terminal` is `!Send`, so every session has one actor thread
//! that owns it. Separate threads read from the holder and write to it; they
//! talk to the actor over a channel. Because only the actor publishes a
//! session's output, it can take a snapshot and subscribe a client in one
//! step, with no output lost or duplicated between the two.

use std::collections::{BTreeMap, HashMap};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, anyhow, bail};
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::selection::Selection;
use libghostty_vt::terminal::{Point, PointCoordinate};
use libghostty_vt::{Terminal, TerminalOptions};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use crate::claude_status;
use crate::holder::{self, Connection, Spec};
use crate::hooks::{self, Hooks};
use crate::status::{self, Agent, Source, State, Status, View};
use crate::utf8::Utf8Stream;
use crate::workspace::dates::now_millis;

const MAX_SCROLLBACK_LINES: usize = 10_000;
/// How often the actor checks quiet time and hook state.
const TICK: Duration = Duration::from_millis(250);
/// No output for this long counts as quiet (agent likely waiting).
const QUIET_AFTER: Duration = Duration::from_secs(2);
/// Output this soon after a keystroke is its echo, not the agent working.
const ECHO_WINDOW: Duration = Duration::from_millis(500);
const EVENT_CAPACITY: usize = 4096;
/// How often a session with new output saves its screen with its holder, for
/// a new core to start from if this one crashes.
const SAVE_SCREEN_EVERY: Duration = Duration::from_secs(120);
/// How long to wait for sessions to save their screens before the core exits.
const SAVE_SCREENS_WAIT: Duration = Duration::from_secs(3);
/// How often a Claude session's own status is compared with its hooks'.
const CLAUDE_CHECK: Duration = Duration::from_secs(1);
/// How often a Claude session's own status is looked for while it's missing.
const CLAUDE_SEARCH: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentState {
    Idle,
    Working,
    Waiting,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionInfo {
    pub id: String,
    pub alive: bool,
    pub exit_code: Option<u32>,
    pub agent: Option<String>,
    /// What the agent is doing (see `status`); none for a plain terminal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// `status` in three words, for clients from before it.
    pub agent_state: Option<AgentState>,
    /// What a waiting agent waits for, in one line, when the core knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_detail: Option<String>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    /// Client-defined tags, stored and returned as given.
    pub labels: BTreeMap<String, String>,
}

impl SessionInfo {
    /// Take a status, and say it the older way too.
    fn set_status(&mut self, status: Status) {
        let view = status.view();
        let state = view.agent_state();
        self.agent_state = Some(state);
        self.agent_detail = view.detail.filter(|_| state == AgentState::Waiting);
        self.status = Some(status);
    }
}

/// What a session's agent reports through, and what it reported before this
/// core picked the session up.
struct Reports {
    state_file: Option<PathBuf>,
    log: Option<hooks::Log>,
    hooked: Option<Agent>,
}

impl Reports {
    /// For a session that starts now.
    fn new(state_file: Option<PathBuf>) -> Self {
        Self {
            log: state_file.as_deref().map(hooks::Log::new),
            state_file,
            hooked: None,
        }
    }

    /// For a session that's been running: its hook log from the start.
    fn read(state_file: Option<PathBuf>) -> Self {
        let mut reports = Self::new(state_file);
        let records = reports.log.as_mut().map(hooks::Log::read_new);
        if let Some(records) = records.filter(|records| !records.is_empty()) {
            let mut agent = Agent::default();
            for record in &records {
                agent.apply(record);
            }
            reports.hooked = Some(agent);
        }
        reports
    }

    /// The status they add up to, where it's from, and since when.
    fn status(&self) -> (View, Source, i64) {
        if let Some(agent) = &self.hooked {
            return (agent.view(), agent.source(), agent.at);
        }
        let word = self.state_file.as_deref().and_then(|path| {
            let state = hooks::read_state(path)?;
            Some(status::of_state_word(
                state,
                hooks::read_input(path).as_ref(),
            ))
        });
        match word {
            Some(view) => (view, Source::Hooks, now_millis()),
            None => (
                View::new(State::Starting, None, None),
                Source::Core,
                now_millis(),
            ),
        }
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum Event {
    SessionCreated {
        session: SessionInfo,
    },
    Output {
        id: Arc<str>,
        data: Arc<str>,
    },
    #[serde(rename_all = "camelCase")]
    SessionExited {
        id: Arc<str>,
        exit_code: Option<u32>,
    },
    SessionRemoved {
        id: Arc<str>,
    },
    /// An agent session's status changed.
    SessionStatus {
        id: Arc<str>,
        status: Status,
    },
    /// `SessionStatus` for clients from before it, sent after it when the
    /// state changes. `detail` (what it waits for) can come with `waiting`;
    /// `seq` is that status's.
    AgentState {
        id: Arc<str>,
        state: AgentState,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        seq: Option<u64>,
    },
    /// Someone typed in a session whose agent is waiting (once each time it
    /// waits): they're answering, so nobody needs a notification.
    SessionTyped {
        id: Arc<str>,
    },
    /// The session produced no output for a while (e.g. an agent finished a reply).
    SessionQuiet {
        id: Arc<str>,
    },
    /// The PTY took another size (another client typed or resized).
    SessionResized {
        id: Arc<str>,
        cols: u16,
        rows: u16,
    },
    /// Workspace files changed (`tasks`, `projects`, `plans`, `daily`, `weekly`).
    WorkspaceChanged {
        areas: Vec<String>,
    },
    /// Progress installing a core on another machine (`POST /v0/fleet/install`).
    InstallProgress {
        host: String,
        step: String,
    },
    /// An event from a fleet device's own stream (relayed by the home core).
    DeviceEvent {
        device: String,
        event: serde_json::Value,
    },
    /// A fleet device became reachable or unreachable.
    DeviceStatus {
        device: String,
        online: bool,
    },
    /// A task's code moving to or back from another machine: `starting`,
    /// `started`, `returned` or `failed`.
    Handoff {
        task: String,
        status: String,
        branch: Option<String>,
        message: Option<String>,
    },
    /// What a task shows changed (see `activity`); none when the task is
    /// gone, or completed with no agent session left.
    TaskActivity {
        task: String,
        activity: Option<crate::activity::Activity>,
    },
    /// The home role moved: this core took it, gave it away, or was linked
    /// to another home (`GET /v0/home` says where it is now).
    HomeChanged {
        home: String,
        generation: u64,
    },
}

impl Event {
    /// The session an event is about; empty for workspace events.
    pub fn session_id(&self) -> &str {
        match self {
            Event::WorkspaceChanged { .. }
            | Event::InstallProgress { .. }
            | Event::DeviceEvent { .. }
            | Event::DeviceStatus { .. }
            | Event::Handoff { .. }
            | Event::TaskActivity { .. }
            | Event::HomeChanged { .. } => "",
            Event::SessionCreated { session } => &session.id,
            Event::Output { id, .. }
            | Event::SessionExited { id, .. }
            | Event::SessionRemoved { id }
            | Event::SessionStatus { id, .. }
            | Event::AgentState { id, .. }
            | Event::SessionTyped { id }
            | Event::SessionQuiet { id }
            | Event::SessionResized { id, .. } => id,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpawnRequest {
    pub id: String,
    /// Program and arguments. Empty runs the user's login shell.
    #[serde(default)]
    pub argv: Vec<String>,
    pub cwd: Option<String>,
    /// Full environment for the child. Empty inherits the core's environment.
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default = "default_cols")]
    pub cols: u16,
    #[serde(default = "default_rows")]
    pub rows: u16,
    /// Set to "claude" to report the agent's state (hooks, else output timing).
    pub agent: Option<String>,
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

pub struct Snapshot {
    pub data: String,
    pub info: SessionInfo,
}

/// A client connection (a WebSocket), so the PTY can follow the size of the
/// client in use. `ANONYMOUS` is for plain HTTP requests, whose size isn't
/// remembered.
pub type ClientId = u64;
pub const ANONYMOUS: ClientId = 0;

enum Msg {
    /// Output, and the position in the program's output after it.
    Output(Vec<u8>, u64),
    Exited(Option<u32>),
    /// The holder went away without saying the program exited.
    HolderGone,
    Input(Vec<u8>, ClientId),
    Resize(u16, u16, ClientId),
    /// The client's connection closed.
    Detach(ClientId),
    Snapshot(oneshot::Sender<String>),
    Attach(oneshot::Sender<(String, broadcast::Receiver<Event>)>),
    Repaint(oneshot::Sender<(String, broadcast::Receiver<Event>)>),
    /// Save the screen with the holder; answers once it has been sent.
    SaveScreen(mpsc::Sender<()>),
}

/// What goes to a session's holder, in order, on its writer thread.
enum ToHolder {
    Input(Vec<u8>),
    Resize(u16, u16),
    SaveScreen(u64, String),
    Kill,
    Bye,
    /// Answers once everything before it has been written.
    Flush(mpsc::Sender<()>),
}

struct Session {
    info: Arc<Mutex<SessionInfo>>,
    tx: mpsc::Sender<Msg>,
    holder: mpsc::Sender<ToHolder>,
}

/// What a holder keeps for the core and gives back to a new one. Fields can
/// only be added: a running holder may have been started by any earlier release.
#[derive(Serialize, Deserialize)]
struct SessionMeta {
    id: String,
    #[serde(default)]
    agent: Option<String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
}

pub struct Registry {
    sessions: Mutex<HashMap<String, Session>>,
    events: broadcast::Sender<Event>,
    hooks: Hooks,
    next_client: AtomicU64,
    /// Where holders listen; each socket is a running session.
    holders: PathBuf,
    /// The binary to run as a holder.
    exe: PathBuf,
}

/// Lock a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 200
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Expand a leading `~/` to the home directory.
pub fn expand_home(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ if path == "~" => std::env::var_os("HOME").map_or_else(|| path.into(), PathBuf::from),
        _ => PathBuf::from(path),
    }
}

impl Registry {
    /// `holders` is the folder holders listen in; `exe` the binary to run as one.
    pub fn new(hooks: Hooks, holders: PathBuf, exe: PathBuf) -> anyhow::Result<Self> {
        std::fs::create_dir_all(&holders)
            .with_context(|| format!("creating {}", holders.display()))?;
        std::fs::set_permissions(&holders, std::fs::Permissions::from_mode(0o700))?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Ok(Self {
            sessions: Mutex::new(HashMap::new()),
            events,
            hooks,
            next_client: AtomicU64::new(ANONYMOUS + 1),
            holders,
            exe,
        })
    }

    /// Pick up the sessions whose holders outlived the previous core, and
    /// return how many. Run before serving, so the first client sees them all.
    pub fn adopt(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.holders) else {
            return 0;
        };
        let mut adopted = 0;
        for path in entries.flatten().map(|entry| entry.path()) {
            if path.extension().is_none_or(|ext| ext != "sock") {
                continue;
            }
            match holder::connect(&path) {
                Ok(connection) => match self.adopt_session(connection) {
                    Ok(()) => adopted += 1,
                    Err(err) => {
                        tracing::warn!(error = %format!("{err:#}"), "couldn't pick up a session");
                    }
                },
                // Nothing is listening: its holder has gone
                Err(err)
                    if matches!(
                        err.kind(),
                        std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
                    ) =>
                {
                    let _ = std::fs::remove_file(&path);
                }
                Err(err) => {
                    tracing::warn!(%err, socket = %path.display(), "couldn't reach a holder");
                }
            }
        }
        adopted
    }

    fn adopt_session(&self, mut connection: Connection) -> anyhow::Result<()> {
        let meta: SessionMeta = serde_json::from_value(connection.welcome.meta.clone())
            .context("reading the holder's record")?;
        let mut sessions = lock(&self.sessions);
        if sessions.contains_key(&meta.id) {
            let _ = holder::write_frame(&mut connection.stream, holder::KILL, &[]);
            bail!("two holders for session {:?}", meta.id);
        }
        let state_file =
            hooks::reports_state(meta.agent.as_deref()).then(|| self.hooks.state_file(&meta.id));
        // What its agent reported so far, to the previous core or to nobody
        let reports = Reports::read(state_file);
        let mut info = SessionInfo {
            id: meta.id.clone(),
            alive: true,
            exit_code: None,
            status: None,
            agent_state: None,
            agent_detail: None,
            agent: meta.agent,
            pid: connection.welcome.pid,
            cols: connection.welcome.cols,
            rows: connection.welcome.rows,
            labels: meta.labels,
        };
        if info.agent.is_some() {
            let (view, source, at) = reports.status();
            info.set_status(Status::next(view, source, at, None));
        }
        let session = self.run_session(connection, info, reports, false)?;
        tracing::info!(id = %meta.id, "picked up a running session");
        sessions.insert(meta.id, session);
        Ok(())
    }

    /// An id for a new client connection.
    pub fn connect(&self) -> ClientId {
        self.next_client.fetch_add(1, Ordering::Relaxed)
    }

    /// The event channel, for other parts of the core that publish events.
    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.events.clone()
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.events.subscribe()
    }

    pub fn list(&self) -> Vec<SessionInfo> {
        let mut list: Vec<_> = lock(&self.sessions)
            .values()
            .map(|s| lock(&s.info).clone())
            .collect();
        list.sort_by(|a, b| a.id.cmp(&b.id));
        list
    }

    pub fn info(&self, id: &str) -> Option<SessionInfo> {
        lock(&self.sessions).get(id).map(|s| lock(&s.info).clone())
    }

    /// Start a session. A live session with the same id is kept as is; a dead
    /// one is replaced.
    pub fn spawn(&self, req: SpawnRequest) -> anyhow::Result<SessionInfo> {
        if !valid_id(&req.id) {
            bail!(
                "invalid session id {:?}: use letters, digits, '-', '_' and '.'",
                req.id
            );
        }
        if req.cols == 0 || req.rows == 0 {
            bail!("cols and rows must be greater than zero");
        }
        let mut sessions = lock(&self.sessions);
        if let Some(existing) = sessions.get(&req.id) {
            let info = lock(&existing.info).clone();
            if info.alive {
                return Ok(info);
            }
        }

        let argv = if req.argv.is_empty() {
            // The user's login shell
            let shell = req
                .env
                .get("SHELL")
                .cloned()
                .or_else(|| std::env::var("SHELL").ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/sh".into());
            vec![shell, "-l".into()]
        } else {
            req.argv
        };
        let clear_env = !req.env.is_empty();
        let mut env: BTreeMap<String, String> = req.env.into_iter().collect();
        let state_file = if hooks::reports_state(req.agent.as_deref()) {
            let (hook_env, state_file) = self.hooks.env_for(&req.id, &self.exe);
            env.extend(hook_env);
            Some(state_file)
        } else {
            None
        };
        let cwd = req
            .cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(expand_home);

        let meta = SessionMeta {
            id: req.id.clone(),
            agent: req.agent.clone(),
            labels: req.labels.clone(),
        };
        // Named at random, not after the session: ids can be longer than a
        // socket path is allowed to be
        let socket = self.holders.join(format!(
            "{}.sock",
            &uuid::Uuid::new_v4().simple().to_string()[..16]
        ));
        holder::launch(
            &self.exe,
            &socket,
            &Spec {
                argv,
                cwd: cwd.clone(),
                env,
                clear_env,
                cols: req.cols,
                rows: req.rows,
                meta: serde_json::to_value(&meta)?,
            },
        )?;
        let connection = holder::connect(&socket).context("connecting to the terminal's holder")?;
        let pid = connection.welcome.pid;

        let mut info = SessionInfo {
            id: req.id.clone(),
            alive: true,
            exit_code: None,
            agent: req.agent,
            status: None,
            agent_state: None,
            agent_detail: None,
            pid,
            cols: req.cols,
            rows: req.rows,
            labels: req.labels,
        };
        if info.agent.is_some() {
            let starting = View::new(State::Starting, None, None);
            info.set_status(Status::next(starting, Source::Core, now_millis(), None));
        }
        let reports = Reports::new(state_file);
        let session = self.run_session(connection, info.clone(), reports, true)?;
        sessions.insert(req.id.clone(), session);
        drop(sessions);
        tracing::info!(id = %req.id, ?pid, cwd = ?cwd, "session started");
        let _ = self.events.send(Event::SessionCreated {
            session: info.clone(),
        });
        Ok(info)
    }

    /// Start the threads of a session whose holder is connected. For a new
    /// session the output so far is news; for one picked up it's only history
    /// to rebuild the screen from.
    fn run_session(
        &self,
        connection: Connection,
        info: SessionInfo,
        reports: Reports,
        new: bool,
    ) -> anyhow::Result<Session> {
        let Connection {
            stream,
            saved_screen,
            replay,
            position,
            ..
        } = connection;
        let id = info.id.clone();
        let (cols, rows) = (info.cols, info.rows);
        let shared = Arc::new(Mutex::new(info));
        let (tx, rx) = mpsc::channel();
        let mut history = Vec::new();
        if new {
            if !replay.is_empty() {
                let _ = tx.send(Msg::Output(replay, position));
            }
        } else {
            history.extend(saved_screen);
            history.push(replay);
        }

        let reader = stream.try_clone().context("reading from the holder")?;
        let holder = spawn_holder_writer(&id, stream)?;
        let actor = Actor {
            id: Arc::from(id.as_str()),
            info: Arc::clone(&shared),
            events: self.events.clone(),
            holder: holder.clone(),
            position,
            unsaved: false,
            saved_at: Instant::now(),
            state_file: reports.state_file,
            log: reports.log,
            hooked: reports.hooked,
            claude_file: None,
            claude_check: Instant::now(),
            hook_seen: false,
            turn_ended: false,
            state_mtime: None,
            last_output: Instant::now(),
            last_input: Instant::now() - ECHO_WINDOW,
            typed_while_waiting: false,
            quiet_sent: true,
            utf8: Utf8Stream::default(),
            sizes: Vec::new(),
            base_size: (cols, rows),
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name(format!("session:{id}"))
            .spawn(move || actor.run(cols, rows, history, rx, ready_tx))
            .context("starting the session thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("session thread stopped during startup"))?
            .context("creating the terminal")?;
        spawn_holder_reader(&id, reader, tx.clone())?;
        Ok(Session {
            info: shared,
            tx,
            holder,
        })
    }

    pub fn input(&self, id: &str, data: Vec<u8>) -> bool {
        self.input_from(id, ANONYMOUS, data)
    }

    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> bool {
        self.resize_from(id, ANONYMOUS, cols, rows)
    }

    /// Type into a session. If this client sized the session before and
    /// another client changed the size since, the PTY takes this client's size back.
    pub fn input_from(&self, id: &str, client: ClientId, data: Vec<u8>) -> bool {
        self.send(id, Msg::Input(data, client))
    }

    /// Resize a session; the size is remembered for this client.
    pub fn resize_from(&self, id: &str, client: ClientId, cols: u16, rows: u16) -> bool {
        cols > 0 && rows > 0 && self.send(id, Msg::Resize(cols, rows, client))
    }

    /// A client's connection closed: if the session has its size, it goes back
    /// to the size of the client that used it before.
    pub fn detach(&self, id: &str, client: ClientId) {
        if client != ANONYMOUS {
            self.send(id, Msg::Detach(client));
        }
    }

    /// Kill a session's process and forget the session.
    pub fn remove(&self, id: &str) -> bool {
        let Some(session) = lock(&self.sessions).remove(id) else {
            return false;
        };
        if lock(&session.info).alive {
            let _ = session.holder.send(ToHolder::Kill);
        }
        tracing::info!(id, "session removed");
        let _ = self
            .events
            .send(Event::SessionRemoved { id: Arc::from(id) });
        true
    }

    pub async fn snapshot(&self, id: &str) -> Option<Snapshot> {
        let (reply, rx) = oneshot::channel();
        if !self.send(id, Msg::Snapshot(reply)) {
            return None;
        }
        let data = rx.await.ok()?;
        Some(Snapshot {
            data,
            info: self.info(id)?,
        })
    }

    /// The current screen plus a receiver that gets every event after it.
    pub async fn attach(&self, id: &str) -> Option<(String, broadcast::Receiver<Event>)> {
        let (reply, rx) = oneshot::channel();
        if !self.send(id, Msg::Attach(reply)) {
            return None;
        }
        rx.await.ok()
    }

    /// Like `attach`, for a viewer that fell behind: output that brings its
    /// terminal to the current screen, keeping its scrollback.
    pub async fn repaint(&self, id: &str) -> Option<(String, broadcast::Receiver<Event>)> {
        let (reply, rx) = oneshot::channel();
        if !self.send(id, Msg::Repaint(reply)) {
            return None;
        }
        rx.await.ok()
    }

    /// End every session's program. The core doesn't do this when it stops:
    /// sessions keep running for the next core to pick up.
    pub fn kill_all(&self) {
        for session in lock(&self.sessions).values() {
            let _ = session.holder.send(ToHolder::Kill);
        }
    }

    /// Save every running session's screen with its holder, for the next core.
    pub fn save_screens(&self) {
        let waiting: Vec<_> = lock(&self.sessions)
            .values()
            .filter_map(|session| {
                let (done, finished) = mpsc::channel();
                session.tx.send(Msg::SaveScreen(done)).ok()?;
                Some(finished)
            })
            .collect();
        let deadline = Instant::now() + SAVE_SCREENS_WAIT;
        for finished in waiting {
            let _ = finished.recv_timeout(deadline.saturating_duration_since(Instant::now()));
        }
    }

    fn send(&self, id: &str, msg: Msg) -> bool {
        lock(&self.sessions)
            .get(id)
            .is_some_and(|s| s.tx.send(msg).is_ok())
    }
}

fn spawn_holder_reader(
    id: &str,
    mut stream: UnixStream,
    tx: mpsc::Sender<Msg>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name(format!("read:{id}"))
        .spawn(move || {
            loop {
                let Ok((kind, payload)) = holder::read_frame(&mut stream) else {
                    let _ = tx.send(Msg::HolderGone);
                    return;
                };
                match kind {
                    holder::OUTPUT => {
                        let Some((at, output)) = holder::split_position(&payload) else {
                            continue;
                        };
                        let end = at + output.len() as u64;
                        if tx.send(Msg::Output(output.to_vec(), end)).is_err() {
                            return;
                        }
                    }
                    holder::EXITED => {
                        let code = serde_json::from_slice::<holder::Exited>(&payload)
                            .ok()
                            .and_then(|exited| exited.code);
                        let _ = tx.send(Msg::Exited(code));
                    }
                    _ => {}
                }
            }
        })
        .context("starting the read thread")?;
    Ok(())
}

/// Writes happen on their own thread: a blocked write must never stall the
/// actor, or a child blocked on output would deadlock against it.
fn spawn_holder_writer(id: &str, mut stream: UnixStream) -> anyhow::Result<mpsc::Sender<ToHolder>> {
    let (tx, rx) = mpsc::channel::<ToHolder>();
    std::thread::Builder::new()
        .name(format!("write:{id}"))
        .spawn(move || {
            for msg in rx {
                let written = match msg {
                    ToHolder::Input(data) => holder::write_frame(&mut stream, holder::INPUT, &data),
                    ToHolder::Resize(cols, rows) => {
                        let [c1, c2] = cols.to_be_bytes();
                        let [r1, r2] = rows.to_be_bytes();
                        holder::write_frame(&mut stream, holder::RESIZE, &[c1, c2, r1, r2])
                    }
                    ToHolder::SaveScreen(position, screen) => holder::write_frame(
                        &mut stream,
                        holder::SAVE_SCREEN,
                        &holder::at_position(position, screen.as_bytes()),
                    ),
                    ToHolder::Kill => holder::write_frame(&mut stream, holder::KILL, &[]),
                    ToHolder::Bye => holder::write_frame(&mut stream, holder::BYE, &[]),
                    ToHolder::Flush(done) => {
                        let _ = done.send(());
                        Ok(())
                    }
                };
                if written.is_err() {
                    break;
                }
            }
        })
        .context("starting the write thread")?;
    Ok(tx)
}

struct Actor {
    id: Arc<str>,
    info: Arc<Mutex<SessionInfo>>,
    events: broadcast::Sender<Event>,
    holder: mpsc::Sender<ToHolder>,
    /// The position in the program's output that `term` has reached.
    position: u64,
    /// There's output the holder's saved screen doesn't cover.
    unsaved: bool,
    saved_at: Instant,
    state_file: Option<PathBuf>,
    /// The session's hook log.
    log: Option<hooks::Log>,
    /// The agent as its hook log tells it, once the log has something.
    hooked: Option<Agent>,
    /// Claude's own status file for the session it has open now, and the
    /// process it belongs to.
    claude_file: Option<(PathBuf, u32)>,
    /// When to compare with Claude's own status next.
    claude_check: Instant,
    /// A state word was reported at least once (Claude's hooks from before
    /// the log); stop guessing the state from output.
    hook_seen: bool,
    /// Codex reported the end of a turn; what it draws until the next input
    /// isn't new work.
    turn_ended: bool,
    state_mtime: Option<SystemTime>,
    last_output: Instant,
    last_input: Instant,
    /// Someone typed since the agent started waiting.
    typed_while_waiting: bool,
    quiet_sent: bool,
    utf8: Utf8Stream,
    /// Each client's last requested size, the client in use last.
    sizes: Vec<(ClientId, u16, u16)>,
    /// The size asked for without a connection (at spawn, or over HTTP), for
    /// when every client that sized the session has gone.
    base_size: (u16, u16),
}

impl Actor {
    fn run(
        mut self,
        cols: u16,
        rows: u16,
        history: Vec<Vec<u8>>,
        rx: mpsc::Receiver<Msg>,
        ready: mpsc::Sender<Result<(), libghostty_vt::Error>>,
    ) {
        let mut term = match Terminal::new(TerminalOptions {
            cols,
            rows,
            max_scrollback: MAX_SCROLLBACK_LINES,
        }) {
            Ok(term) => term,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        for bytes in history {
            term.vt_write(&bytes);
        }
        let _ = ready.send(Ok(()));

        // Set once the holder says the program has exited, or goes away
        let mut ended: Option<Option<u32>> = None;
        let mut next_tick = Instant::now() + TICK;
        loop {
            match rx.recv_timeout(next_tick.saturating_duration_since(Instant::now())) {
                Ok(Msg::Output(first, mut position)) => {
                    let mut bytes = first;
                    // Coalesce whatever else is already queued into one event.
                    let mut deferred = None;
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Output(more, end) => {
                                bytes.extend_from_slice(&more);
                                position = end;
                            }
                            other => {
                                deferred = Some(other);
                                break;
                            }
                        }
                    }
                    self.on_output(&mut term, &bytes, position);
                    if let Some(msg) = deferred {
                        self.handle(&mut term, msg, &mut ended);
                    }
                }
                Ok(msg) => self.handle(&mut term, msg, &mut ended),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            // On a schedule, not only when nothing arrives: an agent that
            // keeps drawing would otherwise never have its hook state read
            if Instant::now() >= next_tick {
                self.tick();
                next_tick = Instant::now() + TICK;
            }
            if let Some(code) = ended {
                self.finish(code);
                break;
            }
            if self.unsaved && self.saved_at.elapsed() >= SAVE_SCREEN_EVERY {
                self.save_screen(&term);
            }
        }
        // Keep answering snapshot requests for the dead session until it's removed.
        while let Ok(msg) = rx.recv() {
            match msg {
                Msg::Snapshot(reply) => {
                    let _ = reply.send(vt_snapshot(&term));
                }
                Msg::Attach(reply) => {
                    let _ = reply.send((vt_snapshot(&term), self.events.subscribe()));
                }
                Msg::Repaint(reply) => {
                    let _ = reply.send((vt_repaint(&term), self.events.subscribe()));
                }
                _ => {}
            }
        }
    }

    fn handle(&mut self, term: &mut Terminal, msg: Msg, ended: &mut Option<Option<u32>>) {
        match msg {
            Msg::Output(bytes, position) => self.on_output(term, &bytes, position),
            Msg::Exited(code) => *ended = Some(code),
            Msg::HolderGone => {
                // Also how a removed session's holder ends, so not a warning
                tracing::info!(id = %self.id, "the session's holder went away");
                *ended = Some(None);
            }
            Msg::SaveScreen(done) => {
                self.save_screen(term);
                let _ = self.holder.send(ToHolder::Flush(done));
            }
            Msg::Input(data, client) => {
                // The client typing gets its size back before its keys arrive
                // (not for its terminal's answers to queries)
                let typing = is_typing(&data);
                if typing {
                    self.typed_in_wait();
                    if let Some(at) = self.sizes.iter().position(|s| s.0 == client) {
                        let size = self.sizes.remove(at);
                        self.sizes.push(size);
                        self.apply_size(term, size.1, size.2);
                    }
                }
                self.last_input = Instant::now();
                // Only typing starts a turn; what a terminal sends by itself
                // (focus, answers to queries) is followed by a repaint, not work
                if typing && std::mem::take(&mut self.turn_ended) {
                    // So a new core doesn't take it for the current turn's end
                    if let Some(path) = &self.state_file {
                        let _ = std::fs::remove_file(path);
                    }
                }
                let _ = self.holder.send(ToHolder::Input(data));
            }
            Msg::Resize(cols, rows, client) => {
                if client == ANONYMOUS {
                    self.base_size = (cols, rows);
                } else {
                    self.sizes.retain(|s| s.0 != client);
                    self.sizes.push((client, cols, rows));
                }
                self.apply_size(term, cols, rows);
            }
            Msg::Detach(client) => {
                let had_size = self.sizes.last().is_some_and(|s| s.0 == client);
                self.sizes.retain(|s| s.0 != client);
                if had_size {
                    let (cols, rows) = self.sizes.last().map_or(self.base_size, |s| (s.1, s.2));
                    self.apply_size(term, cols, rows);
                }
            }
            Msg::Snapshot(reply) => {
                let _ = reply.send(vt_snapshot(term));
            }
            Msg::Attach(reply) => {
                // Subscribe before replying: every later event follows the snapshot.
                let _ = reply.send((vt_snapshot(term), self.events.subscribe()));
            }
            Msg::Repaint(reply) => {
                let _ = reply.send((vt_repaint(term), self.events.subscribe()));
            }
        }
    }

    fn apply_size(&self, term: &mut Terminal, cols: u16, rows: u16) {
        {
            let info = lock(&self.info);
            if (info.cols, info.rows) == (cols, rows) {
                return;
            }
        }
        let _ = self.holder.send(ToHolder::Resize(cols, rows));
        if let Err(err) = term.resize(cols, rows, 0, 0) {
            tracing::warn!(id = %self.id, ?err, "terminal resize failed");
        }
        let mut info = lock(&self.info);
        info.cols = cols;
        info.rows = rows;
        drop(info);
        let _ = self.events.send(Event::SessionResized {
            id: Arc::clone(&self.id),
            cols,
            rows,
        });
    }

    /// Give the holder the screen as it is now, for the next core to start from.
    fn save_screen(&mut self, term: &Terminal) {
        let _ = self
            .holder
            .send(ToHolder::SaveScreen(self.position, vt_snapshot(term)));
        self.saved_at = Instant::now();
        self.unsaved = false;
    }

    fn on_output(&mut self, term: &mut Terminal, bytes: &[u8], position: u64) {
        term.vt_write(bytes);
        self.position = position;
        self.unsaved = true;
        self.last_output = Instant::now();
        self.quiet_sent = false;
        if self.guessing_state() && !self.turn_ended && self.last_input.elapsed() > ECHO_WINDOW {
            self.publish(
                View::new(State::Working, Some("turn"), None),
                Source::Inferred,
                now_millis(),
            );
        }
        let text = self.utf8.push(bytes);
        if !text.is_empty() {
            let _ = self.events.send(Event::Output {
                id: Arc::clone(&self.id),
                data: text.into(),
            });
        }
    }

    fn tick(&mut self) {
        if !self.quiet_sent && self.last_output.elapsed() >= QUIET_AFTER {
            self.quiet_sent = true;
            if self.guessing_state() {
                // Finished or asking: output can't tell
                self.publish(
                    View::new(State::Ready, Some("quiet"), None),
                    Source::Inferred,
                    now_millis(),
                );
            }
            let _ = self.events.send(Event::SessionQuiet {
                id: Arc::clone(&self.id),
            });
        }
        self.poll_log();
        self.poll_hook_state();
        self.check_claude();
    }

    /// Take in what the agent's hooks logged since the last look.
    fn poll_log(&mut self) {
        let records = self.log.as_mut().map(hooks::Log::read_new);
        let Some(records) = records.filter(|records| !records.is_empty()) else {
            return;
        };
        let agent = self.hooked.get_or_insert_with(Agent::default);
        for record in &records {
            agent.apply(record);
        }
        let (view, source, at) = (agent.view(), agent.source(), agent.at);
        self.publish(view, source, at);
    }

    /// Let Claude's own status correct what its hooks couldn't report: a
    /// turn that was interrupted, a permission prompt that was answered.
    fn check_claude(&mut self) {
        if self.claude_check > Instant::now() {
            return;
        }
        self.claude_check = Instant::now() + CLAUDE_CHECK;
        let Some(agent) = &mut self.hooked else {
            return;
        };
        let Some(session) = agent.session.clone() else {
            return;
        };
        if agent.view().state == State::Ended {
            self.claude_file = None;
            return;
        }
        // Its process is what says whether it's still there: the file can
        // be missing for a moment while Claude writes it
        if let Some((_, pid)) = &self.claude_file
            && !claude_status::running(*pid)
        {
            self.claude_file = None;
            self.claude_gone();
            return;
        }
        let known = self.claude_file.as_ref();
        let entry = known.and_then(|(file, _)| claude_status::read(file));
        let entry = match entry.filter(|entry| entry.session == session) {
            Some(entry) => entry,
            None => {
                // Not looked for yet, or its process has another session
                // open now (/clear, /resume)
                let found = claude_status::sessions_dir()
                    .and_then(|dir| claude_status::find(&dir, &session))
                    .and_then(|file| Some((claude_status::read(&file)?, file)))
                    // A file left behind by a Claude that was killed
                    .filter(|(entry, _)| claude_status::running(entry.pid));
                let Some((entry, file)) = found else {
                    self.claude_check = Instant::now() + CLAUDE_SEARCH;
                    return;
                };
                self.claude_file = Some((file, entry.pid));
                entry
            }
        };
        let Some(claude) = entry.status else { return };
        let Some(agent) = &mut self.hooked else {
            return;
        };
        if let Some(record) = status::correction(agent, &claude, now_millis()) {
            agent.apply(&record);
            let (view, source, at) = (agent.view(), agent.source(), agent.at);
            self.publish(view, source, at);
        }
    }

    /// Claude's process ended without its hooks saying so (it was killed).
    fn claude_gone(&mut self) {
        let Some(agent) = &mut self.hooked else {
            return;
        };
        agent.apply(&status::Record {
            t: now_millis(),
            event: status::CLAUDE_GONE.into(),
            ..status::Record::default()
        });
        let (view, source, at) = (agent.view(), agent.source(), agent.at);
        self.publish(view, source, at);
    }

    /// The state word of hooks from before the log, and of Codex.
    fn poll_hook_state(&mut self) {
        if self.hooked.is_some() {
            return;
        }
        let Some(path) = &self.state_file else { return };
        let Some(mtime) = std::fs::metadata(path).and_then(|m| m.modified()).ok() else {
            return;
        };
        if self.state_mtime == Some(mtime) {
            return;
        }
        self.state_mtime = Some(mtime);
        if let Some(state) = hooks::read_state(path) {
            // Claude's hooks report every change; Codex only says when a turn ends
            let claude = lock(&self.info).agent.as_deref() == Some("claude");
            self.hook_seen = claude;
            self.turn_ended = !claude;
            let view = status::of_state_word(state, hooks::read_input(path).as_ref());
            self.publish(view, Source::Hooks, now_millis());
        }
    }

    /// Agent sessions without hook reports (remote, or hooks not loaded) fall
    /// back to output timing: quiet means its turn is over, fresh output
    /// means working.
    fn guessing_state(&self) -> bool {
        self.hooked.is_none() && !self.hook_seen && lock(&self.info).agent.is_some()
    }

    /// Give the session a status, if it says something else than the one it
    /// has, and tell the clients. `at` is when it took it.
    fn publish(&mut self, view: View, source: Source, at: i64) {
        let mut info = lock(&self.info);
        if info.status.as_ref().is_some_and(|now| now.view() == view) {
            return;
        }
        let before = info.agent_state;
        let status = Status::next(view, source, at, info.status.as_ref());
        info.set_status(status.clone());
        let (state, detail) = (info.agent_state, info.agent_detail.clone());
        drop(info);
        self.typed_while_waiting = false;
        let seq = status.seq;
        let _ = self.events.send(Event::SessionStatus {
            id: Arc::clone(&self.id),
            status,
        });
        if let Some(state) = state.filter(|_| state != before) {
            let _ = self.events.send(Event::AgentState {
                id: Arc::clone(&self.id),
                state,
                detail,
                seq: Some(seq),
            });
        }
    }

    /// Say once per wait that someone is typing an answer.
    fn typed_in_wait(&mut self) {
        let waiting = lock(&self.info).agent_state == Some(AgentState::Waiting);
        if waiting && !std::mem::replace(&mut self.typed_while_waiting, true) {
            let _ = self.events.send(Event::SessionTyped {
                id: Arc::clone(&self.id),
            });
        }
    }

    fn finish(&mut self, code: Option<u32>) {
        if lock(&self.info).agent.is_some() {
            // Said like any other status, so no client keeps an older one
            let ended = View::new(State::Ended, None, None);
            self.publish(ended, Source::Core, now_millis());
        }
        {
            let mut info = lock(&self.info);
            info.alive = false;
            info.exit_code = code;
        }
        if let Some(path) = &self.state_file {
            hooks::remove_state(path);
        }
        let _ = self.holder.send(ToHolder::Bye);
        tracing::info!(id = %self.id, ?code, "session exited");
        let _ = self.events.send(Event::SessionExited {
            id: Arc::clone(&self.id),
            exit_code: code,
        });
    }
}

/// Whether input has something a person typed: anything outside escape
/// sequences, or a lone Esc. A terminal's answers to queries (device
/// attributes, cursor reports, colors) are escape sequences only.
fn is_typing(data: &[u8]) -> bool {
    let mut i = 0;
    while i < data.len() {
        if data[i] != 0x1b {
            return true;
        }
        i += 1;
        match data.get(i) {
            None => return true,
            // CSI: parameters, then a final byte
            Some(b'[') => {
                i += 1;
                while i < data.len() && !(0x40..=0x7e).contains(&data[i]) {
                    i += 1;
                }
                i += 1;
            }
            // OSC, DCS, APC: up to BEL or ST
            Some(b']' | b'P' | b'_') => {
                i += 1;
                while i < data.len()
                    && data[i] != 0x07
                    && !(data[i] == 0x1b && data.get(i + 1) == Some(&b'\\'))
                {
                    i += 1;
                }
                i += if data.get(i) == Some(&0x07) { 1 } else { 2 };
            }
            Some(b'O') => i += 2,
            Some(_) => i += 1,
        }
    }
    false
}

/// The terminal's screen and scrollback as VT sequences. Writing this into a
/// fresh xterm-compatible terminal of the same size reproduces the screen,
/// cursor, styles and modes. The palette is left out so clients keep their
/// theme, and tabstops because libghostty emits them after the cursor position
/// (setting them moves the cursor); clients start with the standard 8-column stops.
fn vt_snapshot(term: &Terminal) -> String {
    vt_format(term, None)
}

/// Takes a terminal that missed some output back to a known state without
/// touching its scrollback: leave the alternate screen, soft reset, clear.
const REPAINT_RESET: &str = "\x1b[?1049l\x1b[!p\x1b[H\x1b[2J";

/// The current screen as output for a terminal that missed some: only what's
/// visible, because a snapshot's scrollback can be megabytes.
fn vt_repaint(term: &Terminal) -> String {
    let screen = (|| {
        let corner = |x, y| term.grid_ref(Point::Active(PointCoordinate { x, y }));
        let end = corner(term.cols()? - 1, u32::from(term.rows()?) - 1)?;
        Ok::<_, libghostty_vt::Error>(Selection::new(corner(0, 0)?, end, false))
    })();
    match screen {
        Ok(selection) => format!("{REPAINT_RESET}{}", vt_format(term, Some(&selection))),
        Err(err) => {
            tracing::warn!(?err, "finding the screen for a repaint failed");
            format!("{REPAINT_RESET}{}", vt_snapshot(term))
        }
    }
}

fn vt_format(term: &Terminal, selection: Option<&Selection<'_>>) -> String {
    let mut opts = FormatterOptions::new()
        .with_format(Format::Vt)
        .with_modes(true)
        .with_scrolling_region(true)
        .with_cursor(true)
        .with_style(true)
        .with_hyperlink(true)
        .with_keyboard(true)
        .with_kitty_keyboard(true)
        .with_charsets(true)
        .with_pwd(true);
    if let Some(selection) = selection {
        opts = opts.with_selection(selection);
    }
    match Formatter::new(term, opts).and_then(|mut f| f.format_alloc(None).map(|b| b.to_vec())) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(err) => {
            tracing::warn!(?err, "formatting a snapshot failed");
            String::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(text: &[u8]) -> String {
        let mut term = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 5,
            max_scrollback: 100,
        })
        .expect("terminal");
        term.vt_write(text);
        vt_snapshot(&term)
    }

    fn terminal(text: &[u8]) -> Terminal<'static, 'static> {
        let mut term = Terminal::new(TerminalOptions {
            cols: 40,
            rows: 5,
            max_scrollback: 100,
        })
        .expect("terminal");
        term.vt_write(text);
        term
    }

    /// A viewer that missed output ends up with the same screen after a repaint.
    fn assert_repaint_matches(viewer_had: &[u8], actual: &[u8]) {
        let session = terminal(actual);
        let mut viewer = terminal(viewer_had);
        viewer.vt_write(vt_repaint(&session).as_bytes());
        assert_eq!(vt_repaint(&viewer), vt_repaint(&session));
    }

    #[test]
    fn repaint_should_bring_a_viewer_to_the_current_screen() {
        let lines: String = (0..20).map(|i| format!("line{i}\r\n")).collect();
        assert_repaint_matches(b"older text", lines.as_bytes());
        assert_repaint_matches(b"\x1b[1mbold left on", b"\x1b[31mred\x1b[0m plain");
    }

    #[test]
    fn repaint_should_follow_the_alternate_screen_both_ways() {
        assert_repaint_matches(b"before", b"before\x1b[?1049hin the app");
        assert_repaint_matches(b"before\x1b[?1049hin the app", b"back on the shell");
    }

    #[test]
    fn repaint_should_leave_out_the_scrollback() {
        let lines: String = (0..20).map(|i| format!("line{i}\r\n")).collect();
        let term = terminal(lines.as_bytes());
        let repaint = vt_repaint(&term);
        assert!(repaint.contains("line19"));
        assert!(!repaint.contains("line0"));
    }

    #[test]
    fn repaint_should_keep_the_viewer_s_scrollback() {
        let lines: String = (0..20).map(|i| format!("line{i}\r\n")).collect();
        let mut viewer = terminal(lines.as_bytes());
        viewer.vt_write(vt_repaint(&terminal(b"now")).as_bytes());
        assert!(vt_snapshot(&viewer).contains("line0"));
    }

    #[test]
    fn snapshot_should_contain_written_text() {
        assert!(feed(b"hello \x1b[1mworld\x1b[0m").contains("world"));
    }

    #[test]
    fn snapshot_should_include_scrollback() {
        let lines: String = (0..20).map(|i| format!("line{i}\r\n")).collect();
        assert!(feed(lines.as_bytes()).contains("line0"));
    }

    /// A new core rebuilds its mirror from a snapshot, so a snapshot of the
    /// rebuilt terminal has to match the original.
    fn assert_snapshot_replays(text: &[u8]) {
        let first = feed(text);
        let second = feed(first.as_bytes());
        assert_eq!(first, second);
    }

    #[test]
    fn snapshot_should_replay_the_screen_and_scrollback() {
        let lines: String = (0..20)
            .map(|i| format!("\x1b[3{}mline{i}\r\n", i % 8))
            .collect();
        assert_snapshot_replays(format!("{lines}\x1b[0mprompt$ ").as_bytes());
    }

    #[test]
    fn snapshot_should_replay_modes() {
        assert_snapshot_replays(b"\x1b[?2004h\x1b[?1000h\x1b[?1006h\x1b[?1004h\x1b[>1uhello");
    }

    #[test]
    fn snapshot_should_replay_the_alternate_screen() {
        assert_snapshot_replays(
            b"main screen\r\n\x1b[?1049h\x1b[2J\x1b[Halt screen\x1b[3;5Hcursor",
        );
        let snapshot = feed(b"main\x1b[?1049h\x1b[2J\x1b[Halt");
        assert!(snapshot.contains("alt"));
        assert!(snapshot.contains("\x1b[?1049h"), "{snapshot:?}");
    }

    #[test]
    fn query_answers_should_not_count_as_typing() {
        assert!(!is_typing(b"\x1b[?62;22c"));
        assert!(!is_typing(b"\x1b[12;40R\x1b[?1u"));
        assert!(!is_typing(b"\x1b]11;rgb:0000/0000/0000\x1b\\"));
        assert!(!is_typing(b"\x1bP>|xterm\x1b\\"));
        assert!(is_typing(b"y"));
        assert!(is_typing(b"\r"));
        assert!(is_typing(b"\x1b"));
        assert!(is_typing(b"\x1b[Ax"));
    }

    #[test]
    fn valid_id_should_reject_path_separators() {
        assert!(!valid_id("../etc"));
    }
}
