//! The process that keeps a terminal's program running while the core comes
//! and goes.
//!
//! `tui-do-core hold <socket>` owns one PTY and the program in it. It detaches
//! from the core and serves that one terminal on a Unix socket, so the core
//! can exit (to update, or by crashing) and a new core can pick it up. There's
//! one holder per session: a holder can only ever take down its own terminal,
//! and a new session always runs the current code.
//!
//! A holder only deals in bytes. It keeps the last of the program's output,
//! numbered by position in the stream, and the screen the core last saved
//! with it, with the position that screen covers. A new core rebuilds its
//! mirror from the saved screen plus the output after it.
//!
//! A holder runs the code of the release that started it for as long as its
//! terminal lasts, so the protocol can only grow. Frames are
//! `[kind u8][payload length u32][payload]`, and each side ignores kinds it
//! doesn't know. Never change what a kind means or remove a JSON field.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::OsString;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::Shutdown;
use std::os::fd::AsRawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::{Deserialize, Serialize};

pub const PROTOCOL: u32 = 1;

// Core to holder
pub const HELLO: u8 = 1;
pub const INPUT: u8 = 2;
/// Payload: cols and rows, each a big-endian u16.
pub const RESIZE: u8 = 3;
/// Payload: the position the screen covers (big-endian u64), then the screen.
pub const SAVE_SCREEN: u8 = 4;
pub const KILL: u8 = 5;
/// The core has heard the program exited; the holder can go.
pub const BYE: u8 = 6;

// Holder to core
pub const WELCOME: u8 = 101;
/// Payload: as `SAVE_SCREEN`.
pub const SAVED_SCREEN: u8 = 102;
/// Payload: the position of the first byte (big-endian u64), then the output.
pub const OUTPUT: u8 = 103;
/// Everything from before this connection has been sent.
pub const LIVE: u8 = 104;
pub const EXITED: u8 = 105;

const MAX_FRAME: usize = 64 * 1024 * 1024;
/// Output kept for a new core: enough to cover a restart, and to rebuild the
/// screen if the core crashed.
const RING_BYTES: usize = 2 * 1024 * 1024;
const READ_CHUNK: usize = 64 * 1024;
/// After the program exits, how long to wait for the last output before closing.
const EXIT_DRAIN: Duration = Duration::from_millis(300);
/// How long a holder waits to tell a core its program exited, if none connects.
const LINGER: Duration = Duration::from_secs(24 * 60 * 60);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How often a waiting connection looks at the clock and the connection state.
const WAKE: Duration = Duration::from_millis(100);

/// What a holder runs, sent as JSON on its stdin.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Spec {
    pub argv: Vec<String>,
    pub cwd: Option<PathBuf>,
    /// Added to the holder's environment, or replacing it with `clear_env`.
    pub env: BTreeMap<String, String>,
    pub clear_env: bool,
    pub cols: u16,
    pub rows: u16,
    /// The core's own record of this session, given back on every connection.
    pub meta: serde_json::Value,
}

/// A holder's one line on stdout once it's listening (or couldn't start).
#[derive(Serialize, Deserialize)]
struct Ready {
    error: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct Hello {
    protocol: u32,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Welcome {
    pub protocol: u32,
    pub meta: serde_json::Value,
    pub pid: Option<u32>,
    pub cols: u16,
    pub rows: u16,
}

#[derive(Serialize, Deserialize)]
pub struct Exited {
    pub code: Option<u32>,
}

fn invalid(what: impl ToString) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(5 + payload.len());
    frame.push(kind);
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    w.write_all(&frame)
}

pub fn read_frame(r: &mut impl Read) -> io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    r.read_exact(&mut header)?;
    let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
    if len > MAX_FRAME {
        return Err(invalid("frame too long"));
    }
    let mut payload = vec![0; len];
    r.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// A payload that starts with a position in the output.
pub fn at_position(position: u64, bytes: &[u8]) -> Vec<u8> {
    let mut payload = Vec::with_capacity(8 + bytes.len());
    payload.extend_from_slice(&position.to_be_bytes());
    payload.extend_from_slice(bytes);
    payload
}

pub fn split_position(payload: &[u8]) -> Option<(u64, &[u8])> {
    let (position, bytes) = payload.split_first_chunk::<8>()?;
    Some((u64::from_be_bytes(*position), bytes))
}

/// Start a holder. Returns once its program is running and its socket is
/// listening; `stderr` stays the core's, so holders log to the core's log.
pub fn launch(exe: &Path, socket: &Path, spec: &Spec) -> anyhow::Result<()> {
    let mut child = Command::new(exe)
        .arg("hold")
        .arg(socket)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("starting the terminal's holder")?;
    let mut stdin = child.stdin.take().context("the holder has no stdin")?;
    stdin.write_all(&serde_json::to_vec(spec)?)?;
    drop(stdin);
    let stdout = child.stdout.take().context("the holder has no stdout")?;
    let mut line = String::new();
    BufReader::new(stdout).read_line(&mut line)?;
    // A holder that exits while this core runs has to be waited for
    std::thread::Builder::new()
        .name("reap-holder".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("starting the holder's wait thread")?;
    let ready: Ready = serde_json::from_str(&line).context("the terminal's holder didn't start")?;
    match ready.error {
        Some(error) => bail!("{error}"),
        None => Ok(()),
    }
}

/// A connection to a holder, and what the core starts from.
pub struct Connection {
    pub stream: UnixStream,
    pub welcome: Welcome,
    pub saved_screen: Option<Vec<u8>>,
    pub replay: Vec<u8>,
    /// The position in the output after `saved_screen` and `replay`.
    pub position: u64,
}

/// Connect to a holder, taking over from any core connected before.
pub fn connect(socket: &Path) -> io::Result<Connection> {
    let mut stream = UnixStream::connect(socket)?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let hello = serde_json::to_vec(&Hello { protocol: PROTOCOL }).map_err(invalid)?;
    write_frame(&mut stream, HELLO, &hello)?;
    let (kind, payload) = read_frame(&mut stream)?;
    if kind != WELCOME {
        return Err(invalid(format!("expected a welcome, got frame {kind}")));
    }
    let welcome = serde_json::from_slice(&payload).map_err(invalid)?;
    let (mut saved_screen, mut replay, mut position) = (None, Vec::new(), 0);
    loop {
        let (kind, payload) = read_frame(&mut stream)?;
        match (kind, split_position(&payload)) {
            (SAVED_SCREEN, Some((at, screen))) => {
                saved_screen = Some(screen.to_vec());
                position = at;
            }
            (OUTPUT, Some((at, output))) => {
                replay.extend_from_slice(output);
                position = at + output.len() as u64;
            }
            (LIVE, _) => break,
            _ => {}
        }
    }
    stream.set_read_timeout(None)?;
    Ok(Connection {
        stream,
        welcome,
        saved_screen,
        replay,
        position,
    })
}

/// The last of the program's output.
#[derive(Default)]
struct Ring {
    /// The position in the stream of `bytes[0]`
    start: u64,
    bytes: VecDeque<u8>,
}

impl Ring {
    fn end(&self) -> u64 {
        self.start + self.bytes.len() as u64
    }

    fn push(&mut self, data: &[u8]) {
        self.bytes.extend(data);
        if self.bytes.len() > RING_BYTES {
            let extra = self.bytes.len() - RING_BYTES;
            self.bytes.drain(..extra);
            self.start += extra as u64;
        }
    }

    /// Up to `max` bytes from `from`, or from the oldest byte kept if that's
    /// gone, with the position they start at.
    fn read(&self, from: u64, max: usize) -> (u64, Vec<u8>) {
        let from = from.max(self.start);
        let skip = (from - self.start) as usize;
        let take = self.bytes.len().saturating_sub(skip).min(max);
        (from, self.bytes.range(skip..skip + take).copied().collect())
    }
}

struct State {
    ring: Ring,
    eof: bool,
    exited: Option<(Instant, Option<u32>)>,
    saved_screen: Option<(u64, Vec<u8>)>,
    cols: u16,
    rows: u16,
    connections: u64,
    /// The core being served; a newer connection takes over.
    connection: Option<u64>,
}

impl State {
    /// The exit code once the program has exited and its output has drained
    /// (background jobs can keep the PTY open forever).
    fn finished(&self) -> Option<Option<u32>> {
        self.exited
            .filter(|(at, _)| self.eof || at.elapsed() >= EXIT_DRAIN)
            .map(|(_, code)| code)
    }
}

struct Holder {
    state: Mutex<State>,
    wake: Condvar,
    socket: PathBuf,
    meta: serde_json::Value,
    pid: Option<u32>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    input: mpsc::Sender<Vec<u8>>,
}

/// Lock a mutex, recovering the data if another thread panicked while holding it.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// `tui-do-core hold <socket>`: read the spec, start the program, serve it.
pub fn run(socket: &Path) -> anyhow::Result<()> {
    // Out of the core's process group: launchd ends what's left in a
    // service's group when it exits
    unsafe { libc::setsid() };
    let mut spec = String::new();
    io::stdin().read_to_string(&mut spec)?;
    let spec: Spec = serde_json::from_str(&spec).context("reading the holder's spec")?;
    let started = start(socket, spec);
    let ready = Ready {
        error: started.as_ref().err().map(|err| format!("{err:#}")),
    };
    println!("{}", serde_json::to_string(&ready)?);
    let (holder, listener) = match started {
        Ok(started) => started,
        Err(err) => {
            let _ = std::fs::remove_file(socket);
            return Err(err);
        }
    };
    let null = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/null")?;
    unsafe {
        libc::dup2(null.as_raw_fd(), 0);
        libc::dup2(null.as_raw_fd(), 1);
    }
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let holder = Arc::clone(&holder);
                std::thread::spawn(move || {
                    let _ = holder.serve(stream);
                });
            }
            Err(_) => std::thread::sleep(WAKE),
        }
    }
    Ok(())
}

fn start(socket: &Path, spec: Spec) -> anyhow::Result<(Arc<Holder>, UnixListener)> {
    let listener =
        UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: spec.rows,
            cols: spec.cols,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("opening a PTY")?;
    let mut cmd = CommandBuilder::from_argv(spec.argv.iter().map(OsString::from).collect());
    if spec.clear_env {
        cmd.env_clear();
    }
    for (key, value) in &spec.env {
        cmd.env(key, value);
    }
    if cmd.get_env("TERM").is_none() {
        cmd.env("TERM", "xterm-256color");
    }
    if let Some(cwd) = &spec.cwd {
        cmd.cwd(cwd);
    }
    let child = pair
        .slave
        .spawn_command(cmd)
        .context("starting the session's command")?;
    drop(pair.slave);
    let reader = pair.master.try_clone_reader().context("reading the PTY")?;
    let writer = pair.master.take_writer().context("writing the PTY")?;
    let (input, inputs) = mpsc::channel();

    let holder = Arc::new(Holder {
        state: Mutex::new(State {
            ring: Ring::default(),
            eof: false,
            exited: None,
            saved_screen: None,
            cols: spec.cols,
            rows: spec.rows,
            connections: 0,
            connection: None,
        }),
        wake: Condvar::new(),
        socket: socket.to_path_buf(),
        meta: spec.meta,
        pid: child.process_id(),
        master: Mutex::new(pair.master),
        killer: Mutex::new(child.clone_killer()),
        input,
    });
    let reading = Arc::clone(&holder);
    std::thread::spawn(move || reading.read_pty(reader));
    std::thread::spawn(move || write_pty(writer, inputs));
    let waiting = Arc::clone(&holder);
    std::thread::spawn(move || waiting.wait_for_program(child));
    Ok((holder, listener))
}

/// Writes happen on their own thread: a blocked PTY write must never hold up
/// the connection, or a child blocked on output would deadlock against it.
fn write_pty(mut writer: Box<dyn Write + Send>, inputs: mpsc::Receiver<Vec<u8>>) {
    for data in inputs {
        if writer
            .write_all(&data)
            .and_then(|()| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

/// What a connection sends next.
enum Next {
    Stop,
    Live,
    Output(u64, Vec<u8>),
    Exited(Option<u32>),
}

impl Holder {
    fn read_pty(&self, mut reader: Box<dyn Read + Send>) {
        let mut buf = vec![0u8; READ_CHUNK];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    lock(&self.state).ring.push(&buf[..n]);
                    self.wake.notify_all();
                }
            }
        }
        lock(&self.state).eof = true;
        self.wake.notify_all();
    }

    fn wait_for_program(&self, mut child: Box<dyn Child + Send + Sync>) {
        let code = child.wait().ok().map(|status| status.exit_code());
        let at = Instant::now();
        lock(&self.state).exited = Some((at, code));
        self.wake.notify_all();
        // A core may never come to hear about it
        loop {
            std::thread::sleep(Duration::from_secs(60));
            if at.elapsed() >= LINGER && lock(&self.state).connection.is_none() {
                self.finish();
            }
        }
    }

    fn finish(&self) -> ! {
        let _ = std::fs::remove_file(&self.socket);
        std::process::exit(0);
    }

    fn serve(self: Arc<Self>, mut stream: UnixStream) -> io::Result<()> {
        // Before taking over, so a stray connection can't push out the core
        stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
        let (kind, _) = read_frame(&mut stream)?;
        if kind != HELLO {
            return Ok(());
        }
        stream.set_read_timeout(None)?;

        // What the new core starts from, taken in one step
        let (id, welcome, saved_screen, mut sent, replay_end) = {
            let mut state = lock(&self.state);
            state.connections += 1;
            let id = state.connections;
            state.connection = Some(id);
            let welcome = Welcome {
                protocol: PROTOCOL,
                meta: self.meta.clone(),
                pid: self.pid,
                cols: state.cols,
                rows: state.rows,
            };
            let saved_screen = state.saved_screen.clone();
            let sent = saved_screen.as_ref().map_or(0, |(at, _)| *at);
            let replay_end = state.ring.end();
            (id, welcome, saved_screen, sent, replay_end)
        };
        self.wake.notify_all();

        let reader = stream.try_clone()?;
        let reading = Arc::clone(&self);
        std::thread::spawn(move || reading.read_core(reader, id));

        let welcome = serde_json::to_vec(&welcome).map_err(invalid)?;
        write_frame(&mut stream, WELCOME, &welcome)?;
        if let Some((at, screen)) = &saved_screen {
            write_frame(&mut stream, SAVED_SCREEN, &at_position(*at, screen))?;
        }
        let (mut live, mut told_exit) = (false, false);
        loop {
            match self.next(id, sent, live, replay_end, told_exit) {
                Next::Stop => {
                    let _ = stream.shutdown(Shutdown::Both);
                    return Ok(());
                }
                Next::Live => {
                    write_frame(&mut stream, LIVE, &[])?;
                    live = true;
                }
                Next::Output(at, output) => {
                    write_frame(&mut stream, OUTPUT, &at_position(at, &output))?;
                    sent = at + output.len() as u64;
                }
                Next::Exited(code) => {
                    let exited = serde_json::to_vec(&Exited { code }).map_err(invalid)?;
                    write_frame(&mut stream, EXITED, &exited)?;
                    told_exit = true;
                }
            }
        }
    }

    /// Wait until connection `id` has something to send.
    fn next(&self, id: u64, sent: u64, live: bool, replay_end: u64, told_exit: bool) -> Next {
        let mut state = lock(&self.state);
        loop {
            if state.connection != Some(id) {
                return Next::Stop;
            }
            if !live && sent >= replay_end {
                return Next::Live;
            }
            if state.ring.end() > sent {
                let (at, output) = state.ring.read(sent, READ_CHUNK);
                return Next::Output(at, output);
            }
            if !told_exit && let Some(code) = state.finished() {
                return Next::Exited(code);
            }
            state = self
                .wake
                .wait_timeout(state, WAKE)
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .0;
        }
    }

    fn read_core(&self, mut stream: UnixStream, id: u64) {
        while let Ok((kind, payload)) = read_frame(&mut stream) {
            if lock(&self.state).connection != Some(id) {
                return;
            }
            match kind {
                INPUT => {
                    let _ = self.input.send(payload);
                }
                RESIZE => {
                    if let [c1, c2, r1, r2] = payload[..] {
                        self.resize(u16::from_be_bytes([c1, c2]), u16::from_be_bytes([r1, r2]));
                    }
                }
                SAVE_SCREEN => {
                    if let Some((at, screen)) = split_position(&payload) {
                        lock(&self.state).saved_screen = Some((at, screen.to_vec()));
                    }
                }
                KILL => {
                    let _ = lock(&self.killer).kill();
                    self.finish();
                }
                BYE => self.finish(),
                _ => {}
            }
        }
        let mut state = lock(&self.state);
        if state.connection == Some(id) {
            state.connection = None;
        }
        drop(state);
        self.wake.notify_all();
    }

    fn resize(&self, cols: u16, rows: u16) {
        let _ = lock(&self.master).resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        let mut state = lock(&self.state);
        state.cols = cols;
        state.rows = rows;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A running holder may be from any earlier release. If this fails,
    /// add a new kind instead of changing this one.
    #[test]
    fn frames_should_keep_their_wire_format() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, INPUT, b"hi").expect("writes");
        assert_eq!(bytes, [2, 0, 0, 0, 2, b'h', b'i']);
        assert_eq!(
            (HELLO, INPUT, RESIZE, SAVE_SCREEN, KILL, BYE),
            (1, 2, 3, 4, 5, 6)
        );
        assert_eq!(
            (WELCOME, SAVED_SCREEN, OUTPUT, LIVE, EXITED),
            (101, 102, 103, 104, 105)
        );
        assert_eq!(at_position(258, b"x"), [0, 0, 0, 0, 0, 0, 1, 2, b'x']);
    }

    #[test]
    fn frames_should_read_back() {
        let mut bytes = Vec::new();
        write_frame(&mut bytes, OUTPUT, &at_position(7, b"abc")).expect("writes");
        let (kind, payload) = read_frame(&mut bytes.as_slice()).expect("reads");
        assert_eq!(kind, OUTPUT);
        assert_eq!(split_position(&payload), Some((7, &b"abc"[..])));
    }

    #[test]
    fn oversized_frames_should_be_refused() {
        let header = [OUTPUT, 0xff, 0xff, 0xff, 0xff];
        assert!(read_frame(&mut header.as_slice()).is_err());
    }

    #[test]
    fn ring_should_keep_positions_past_its_size() {
        let mut ring = Ring::default();
        ring.push(&vec![b'a'; RING_BYTES]);
        ring.push(b"bcd");
        assert_eq!(ring.start, 3);
        assert_eq!(ring.end(), RING_BYTES as u64 + 3);
        // The tail reads back from its position
        assert_eq!(
            ring.read(RING_BYTES as u64, 10),
            (RING_BYTES as u64, b"bcd".to_vec())
        );
        // A position that's gone reads from the oldest byte kept
        assert_eq!(ring.read(0, 1).0, 3);
    }

    #[test]
    fn ring_should_read_nothing_at_its_end() {
        let mut ring = Ring::default();
        ring.push(b"abc");
        assert_eq!(ring.read(3, 10), (3, Vec::new()));
    }

    #[test]
    fn welcome_should_read_without_fields_added_later() {
        let json = r#"{"protocol":1,"meta":{"id":"a"},"pid":9,"cols":80,"rows":24,"future":true}"#;
        let welcome: Welcome = serde_json::from_str(json).expect("parses");
        assert_eq!(welcome.pid, Some(9));
    }
}
