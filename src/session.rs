//! Terminal sessions.
//!
//! Each session runs a PTY child and mirrors its output into a headless
//! libghostty terminal, so the core always knows the current screen. A client
//! attaching from anywhere gets that screen as a VT snapshot, then live output.
//!
//! libghostty's `Terminal` is `!Send`, so every session has one actor thread
//! that owns it. Separate threads read the PTY, write to it and wait for the
//! child; they talk to the actor over a channel. Because only the actor
//! publishes a session's output, it can take a snapshot and subscribe a client
//! in one step, with no output lost or duplicated between the two.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, anyhow, bail};
use libghostty_vt::fmt::{Format, Formatter, FormatterOptions};
use libghostty_vt::{Terminal, TerminalOptions};
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, oneshot};

use crate::hooks::{self, Hooks};
use crate::utf8::Utf8Stream;

const MAX_SCROLLBACK_LINES: usize = 10_000;
const READ_CHUNK: usize = 64 * 1024;
/// How often the actor wakes without input to check quiet time and hook state.
const TICK: Duration = Duration::from_millis(250);
/// No output for this long counts as quiet (agent likely waiting).
const QUIET_AFTER: Duration = Duration::from_secs(2);
/// Output this soon after a keystroke is its echo, not the agent working.
const ECHO_WINDOW: Duration = Duration::from_millis(500);
/// After the child exits, how long to wait for the last output before closing.
const EXIT_DRAIN: Duration = Duration::from_millis(300);
const EVENT_CAPACITY: usize = 4096;

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
    pub agent_state: Option<AgentState>,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
    /// Client-defined tags, stored and returned as given.
    pub labels: BTreeMap<String, String>,
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
    AgentState {
        id: Arc<str>,
        state: AgentState,
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
            | Event::HomeChanged { .. } => "",
            Event::SessionCreated { session } => &session.id,
            Event::Output { id, .. }
            | Event::SessionExited { id, .. }
            | Event::SessionRemoved { id }
            | Event::AgentState { id, .. }
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
    Output(Vec<u8>),
    Eof,
    Exited(Option<u32>),
    Input(Vec<u8>, ClientId),
    Resize(u16, u16, ClientId),
    /// The client's connection closed.
    Detach(ClientId),
    Snapshot(oneshot::Sender<String>),
    Attach(oneshot::Sender<(String, broadcast::Receiver<Event>)>),
}

struct Session {
    info: Arc<Mutex<SessionInfo>>,
    tx: mpsc::Sender<Msg>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
}

pub struct Registry {
    sessions: Mutex<HashMap<String, Session>>,
    events: broadcast::Sender<Event>,
    hooks: Hooks,
    next_client: AtomicU64,
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
    pub fn new(hooks: Hooks) -> Self {
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        Self {
            sessions: Mutex::new(HashMap::new()),
            events,
            hooks,
            next_client: AtomicU64::new(ANONYMOUS + 1),
        }
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

        let pair = native_pty_system()
            .openpty(PtySize {
                rows: req.rows,
                cols: req.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("opening a PTY")?;

        let mut cmd = if req.argv.is_empty() {
            // The user's login shell (portable-pty's default-program builder
            // can't take arguments, so name the shell explicitly)
            let shell = req
                .env
                .get("SHELL")
                .cloned()
                .or_else(|| std::env::var("SHELL").ok())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/bin/sh".into());
            let mut cmd = CommandBuilder::new(shell);
            cmd.arg("-l");
            cmd
        } else {
            CommandBuilder::from_argv(req.argv.iter().map(OsString::from).collect())
        };
        if !req.env.is_empty() {
            cmd.env_clear();
            for (key, value) in &req.env {
                cmd.env(key, value);
            }
        }
        if cmd.get_env("TERM").is_none() {
            cmd.env("TERM", "xterm-256color");
        }
        let is_claude = req.agent.as_deref() == Some("claude");
        let state_file = if is_claude {
            let (env, state_file) = self.hooks.env_for(&req.id);
            for (key, value) in env {
                cmd.env(key, value);
            }
            Some(state_file)
        } else {
            None
        };
        let cwd = req
            .cwd
            .as_deref()
            .filter(|c| !c.is_empty())
            .map(expand_home);
        if let Some(cwd) = &cwd {
            cmd.cwd(cwd);
        }

        let mut child = pair
            .slave
            .spawn_command(cmd)
            .context("starting the session's command")?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair.master.try_clone_reader().context("reading the PTY")?;
        let writer = pair.master.take_writer().context("writing the PTY")?;

        let info = SessionInfo {
            id: req.id.clone(),
            alive: true,
            exit_code: None,
            agent: req.agent.clone(),
            agent_state: is_claude.then_some(AgentState::Idle),
            pid,
            cols: req.cols,
            rows: req.rows,
            labels: req.labels,
        };
        let shared = Arc::new(Mutex::new(info.clone()));
        let (tx, rx) = mpsc::channel();

        spawn_reader(&req.id, reader, tx.clone())?;
        let writer_tx = spawn_writer(&req.id, writer)?;
        let waiter_tx = tx.clone();
        std::thread::Builder::new()
            .name(format!("wait:{}", req.id))
            .spawn(move || {
                let code = child.wait().ok().map(|status| status.exit_code());
                let _ = waiter_tx.send(Msg::Exited(code));
            })
            .context("starting the wait thread")?;

        let actor = Actor {
            id: Arc::from(req.id.as_str()),
            info: Arc::clone(&shared),
            events: self.events.clone(),
            master: pair.master,
            writer: writer_tx,
            state_file,
            hook_seen: false,
            state_mtime: None,
            last_output: Instant::now(),
            last_input: Instant::now() - ECHO_WINDOW,
            quiet_sent: true,
            utf8: Utf8Stream::default(),
            sizes: Vec::new(),
            base_size: (req.cols, req.rows),
        };
        let (ready_tx, ready_rx) = mpsc::channel();
        let (cols, rows) = (req.cols, req.rows);
        std::thread::Builder::new()
            .name(format!("session:{}", req.id))
            .spawn(move || actor.run(cols, rows, rx, ready_tx))
            .context("starting the session thread")?;
        ready_rx
            .recv()
            .map_err(|_| anyhow!("session thread stopped during startup"))?
            .context("creating the terminal")?;

        sessions.insert(
            req.id.clone(),
            Session {
                info: shared,
                tx,
                killer: Mutex::new(killer),
            },
        );
        drop(sessions);
        tracing::info!(id = %req.id, ?pid, cwd = ?cwd, "session started");
        let _ = self.events.send(Event::SessionCreated {
            session: info.clone(),
        });
        Ok(info)
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
            let _ = lock(&session.killer).kill();
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

    pub fn kill_all(&self) {
        for session in lock(&self.sessions).values() {
            let _ = lock(&session.killer).kill();
        }
    }

    fn send(&self, id: &str, msg: Msg) -> bool {
        lock(&self.sessions)
            .get(id)
            .is_some_and(|s| s.tx.send(msg).is_ok())
    }
}

fn spawn_reader(
    id: &str,
    mut reader: Box<dyn Read + Send>,
    tx: mpsc::Sender<Msg>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name(format!("read:{id}"))
        .spawn(move || {
            let mut buf = vec![0u8; READ_CHUNK];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(Msg::Output(buf[..n].to_vec())).is_err() {
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Msg::Eof);
        })
        .context("starting the read thread")?;
    Ok(())
}

/// Writes happen on their own thread: a blocked PTY write must never stall the
/// actor, or a child blocked on output would deadlock against it.
fn spawn_writer(
    id: &str,
    mut writer: Box<dyn Write + Send>,
) -> anyhow::Result<mpsc::Sender<Vec<u8>>> {
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::Builder::new()
        .name(format!("write:{id}"))
        .spawn(move || {
            for data in rx {
                if writer
                    .write_all(&data)
                    .and_then(|()| writer.flush())
                    .is_err()
                {
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
    master: Box<dyn MasterPty + Send>,
    writer: mpsc::Sender<Vec<u8>>,
    state_file: Option<PathBuf>,
    /// Hooks reported at least once; stop guessing the state from output.
    hook_seen: bool,
    state_mtime: Option<SystemTime>,
    last_output: Instant,
    last_input: Instant,
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
        let _ = ready.send(Ok(()));

        // Set once the child exits; the session closes when the PTY drains or
        // EXIT_DRAIN passes (background jobs can keep the PTY open forever).
        let mut exited: Option<(Instant, Option<u32>)> = None;
        let mut eof = false;
        loop {
            match rx.recv_timeout(TICK) {
                Ok(Msg::Output(first)) => {
                    let mut bytes = first;
                    // Coalesce whatever else is already queued into one event.
                    let mut deferred = None;
                    while let Ok(msg) = rx.try_recv() {
                        match msg {
                            Msg::Output(more) => bytes.extend_from_slice(&more),
                            other => {
                                deferred = Some(other);
                                break;
                            }
                        }
                    }
                    self.on_output(&mut term, &bytes);
                    if let Some(msg) = deferred {
                        self.handle(&mut term, msg, &mut exited, &mut eof);
                    }
                }
                Ok(msg) => self.handle(&mut term, msg, &mut exited, &mut eof),
                Err(mpsc::RecvTimeoutError::Timeout) => self.tick(),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
            if let Some((at, code)) = exited
                && (eof || at.elapsed() >= EXIT_DRAIN)
            {
                self.finish(code);
                break;
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
                _ => {}
            }
        }
    }

    fn handle(
        &mut self,
        term: &mut Terminal,
        msg: Msg,
        exited: &mut Option<(Instant, Option<u32>)>,
        eof: &mut bool,
    ) {
        match msg {
            Msg::Output(bytes) => self.on_output(term, &bytes),
            Msg::Eof => *eof = true,
            Msg::Exited(code) => *exited = Some((Instant::now(), code)),
            Msg::Input(data, client) => {
                // The client typing gets its size back before its keys arrive
                // (not for its terminal's answers to queries)
                if is_typing(&data)
                    && let Some(at) = self.sizes.iter().position(|s| s.0 == client)
                {
                    let size = self.sizes.remove(at);
                    self.sizes.push(size);
                    self.apply_size(term, size.1, size.2);
                }
                self.last_input = Instant::now();
                let _ = self.writer.send(data);
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
        }
    }

    fn apply_size(&self, term: &mut Terminal, cols: u16, rows: u16) {
        {
            let info = lock(&self.info);
            if (info.cols, info.rows) == (cols, rows) {
                return;
            }
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
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

    fn on_output(&mut self, term: &mut Terminal, bytes: &[u8]) {
        term.vt_write(bytes);
        self.last_output = Instant::now();
        self.quiet_sent = false;
        if self.guessing_state() && self.last_input.elapsed() > ECHO_WINDOW {
            self.set_state(AgentState::Working);
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
                self.set_state(AgentState::Waiting);
            }
            let _ = self.events.send(Event::SessionQuiet {
                id: Arc::clone(&self.id),
            });
        }
        self.poll_hook_state();
    }

    fn poll_hook_state(&mut self) {
        let Some(path) = &self.state_file else { return };
        let Some(mtime) = std::fs::metadata(path).and_then(|m| m.modified()).ok() else {
            return;
        };
        if self.state_mtime == Some(mtime) {
            return;
        }
        self.state_mtime = Some(mtime);
        if let Some(state) = hooks::read_state(path) {
            self.hook_seen = true;
            self.set_state(state);
        }
    }

    /// Agent sessions without hook reports (remote, or hooks not loaded) fall
    /// back to output timing: quiet means waiting, fresh output means working.
    fn guessing_state(&self) -> bool {
        !self.hook_seen && lock(&self.info).agent.is_some()
    }

    fn set_state(&self, state: AgentState) {
        let mut info = lock(&self.info);
        if info.agent_state == Some(state) {
            return;
        }
        info.agent_state = Some(state);
        drop(info);
        let _ = self.events.send(Event::AgentState {
            id: Arc::clone(&self.id),
            state,
        });
    }

    fn finish(&mut self, code: Option<u32>) {
        {
            let mut info = lock(&self.info);
            info.alive = false;
            info.exit_code = code;
            if info.agent.is_some() {
                info.agent_state = Some(AgentState::Idle);
            }
        }
        if let Some(path) = &self.state_file {
            let _ = std::fs::remove_file(path);
        }
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
    let opts = FormatterOptions::new()
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

    #[test]
    fn snapshot_should_contain_written_text() {
        assert!(feed(b"hello \x1b[1mworld\x1b[0m").contains("world"));
    }

    #[test]
    fn snapshot_should_include_scrollback() {
        let lines: String = (0..20).map(|i| format!("line{i}\r\n")).collect();
        assert!(feed(lines.as_bytes()).contains("line0"));
    }

    fn registry() -> Registry {
        let dir = std::env::temp_dir().join(format!("tui-do-core-test-{}", uuid::Uuid::new_v4()));
        Registry::new(Hooks::prepare(&dir).expect("hooks"))
    }

    fn spawn_request(id: &str, argv: &[&str]) -> SpawnRequest {
        SpawnRequest {
            id: id.into(),
            argv: argv.iter().map(|s| (*s).to_string()).collect(),
            cwd: None,
            env: HashMap::new(),
            cols: 80,
            rows: 24,
            agent: None,
            labels: BTreeMap::new(),
        }
    }

    #[test]
    fn spawn_without_argv_should_start_a_login_shell() {
        let registry = registry();
        let info = registry.spawn(spawn_request("shell", &[])).expect("spawns");
        assert!(info.alive);
        registry.kill_all();
    }

    #[tokio::test]
    async fn spawned_command_output_should_reach_the_snapshot() {
        let registry = registry();
        registry
            .spawn(spawn_request(
                "echo",
                &["/bin/sh", "-c", "echo snapshot-check; sleep 5"],
            ))
            .expect("spawns");
        let mut found = false;
        for _ in 0..50 {
            tokio::time::sleep(Duration::from_millis(50)).await;
            if registry
                .snapshot("echo")
                .await
                .is_some_and(|s| s.data.contains("snapshot-check"))
            {
                found = true;
                break;
            }
        }
        registry.kill_all();
        assert!(found);
    }

    #[tokio::test]
    async fn size_should_follow_the_client_in_use() {
        let registry = registry();
        registry
            .spawn(spawn_request("sizes", &["/bin/sh", "-c", "sleep 5"]))
            .expect("spawns");
        let size = |registry: &Registry| {
            let info = registry.info("sizes").expect("session");
            (info.cols, info.rows)
        };
        let (mac, phone) = (registry.connect(), registry.connect());
        registry.resize_from("sizes", mac, 120, 40);
        registry.resize_from("sizes", phone, 50, 30);
        // A snapshot round-trips through the actor, so earlier messages are done
        registry.snapshot("sizes").await.expect("snapshot");
        assert_eq!(size(&registry), (50, 30));

        // The Mac types: it takes its size back
        registry.input_from("sizes", mac, b"x".to_vec());
        registry.snapshot("sizes").await.expect("snapshot");
        assert_eq!(size(&registry), (120, 40));

        // The phone types, then leaves: back to the Mac's size
        registry.input_from("sizes", phone, b"x".to_vec());
        registry.detach("sizes", phone);
        registry.snapshot("sizes").await.expect("snapshot");
        assert_eq!(size(&registry), (120, 40));

        // Once every client that sized it has gone, it's back to its own size
        registry.detach("sizes", mac);
        registry.snapshot("sizes").await.expect("snapshot");
        assert_eq!(size(&registry), (80, 24));
        registry.kill_all();
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
