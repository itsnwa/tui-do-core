//! Terminal sessions, run for real: the actual binary as holder and core.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tui_do_core::hooks::Hooks;
use tui_do_core::session::{Registry, SpawnRequest};

const BINARY: &str = env!("CARGO_BIN_EXE_tui-do-core");

/// A folder for one test. Short, because socket paths are limited to 104
/// bytes and TMPDIR can be long.
struct TestDir(PathBuf);

impl TestDir {
    fn new() -> Self {
        let name = uuid::Uuid::new_v4().simple().to_string();
        let base = std::env::temp_dir();
        let base = if base.as_os_str().len() < 40 {
            base
        } else {
            PathBuf::from("/tmp")
        };
        let dir = base.join(format!("tdc-{}", &name[..8]));
        std::fs::create_dir_all(&dir).expect("creates the test folder");
        Self(dir)
    }

    fn registry(&self) -> Registry {
        Registry::new(
            Hooks::prepare(&self.0.join("claude-hooks"), BINARY.as_ref()).expect("hooks"),
            self.0.join("holders"),
            BINARY.into(),
        )
        .expect("registry")
    }

    fn sockets(&self) -> usize {
        std::fs::read_dir(self.0.join("holders")).map_or(0, |dir| dir.flatten().count())
    }
}

impl Drop for TestDir {
    /// Holders outlive the core, so a test's have to be ended too.
    fn drop(&mut self) {
        let _ = Command::new("/usr/bin/pkill")
            .args(["-f", &format!("hold {}/", self.0.display())])
            .status();
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn request(id: &str, argv: &[&str]) -> SpawnRequest {
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

/// Poll for up to five seconds.
async fn eventually(mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..100 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn shows(registry: &Registry, id: &str, text: &str) -> bool {
    eventually(async || {
        registry
            .snapshot(id)
            .await
            .is_some_and(|s| s.data.contains(text))
    })
    .await
}

#[test]
fn spawn_without_argv_should_start_a_login_shell() {
    let dir = TestDir::new();
    let registry = dir.registry();
    let info = registry.spawn(request("shell", &[])).expect("spawns");
    assert!(info.alive);
    registry.kill_all();
}

#[tokio::test]
async fn spawn_should_report_a_program_that_cannot_start() {
    let dir = TestDir::new();
    let registry = dir.registry();
    assert!(
        registry
            .spawn(request("missing", &["/no/such/program"]))
            .is_err()
    );
    // The holder reports the failure first, then removes its socket
    assert!(eventually(async || dir.sockets() == 0).await);
}

#[test]
fn a_long_session_id_should_fit_a_socket_path() {
    let dir = TestDir::new();
    let registry = dir.registry();
    let id = "a".repeat(200);
    registry
        .spawn(request(&id, &["/bin/sh", "-c", "sleep 5"]))
        .expect("spawns");
    registry.kill_all();
}

#[tokio::test]
async fn spawned_command_output_should_reach_the_snapshot() {
    let dir = TestDir::new();
    let registry = dir.registry();
    registry
        .spawn(request(
            "echo",
            &["/bin/sh", "-c", "echo snapshot-check; sleep 5"],
        ))
        .expect("spawns");
    let found = shows(&registry, "echo", "snapshot-check").await;
    registry.kill_all();
    assert!(found);
}

#[tokio::test]
async fn size_should_follow_the_client_in_use() {
    let dir = TestDir::new();
    let registry = dir.registry();
    registry
        .spawn(request("sizes", &["/bin/sh", "-c", "sleep 5"]))
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

#[tokio::test]
async fn removing_a_session_should_end_its_program_and_holder() {
    let dir = TestDir::new();
    let registry = dir.registry();
    let info = registry
        .spawn(request("gone", &["/bin/sh", "-c", "sleep 30"]))
        .expect("spawns");
    assert_eq!(dir.sockets(), 1);
    assert!(registry.remove("gone"));
    let pid = info.pid.expect("pid") as i32;
    assert!(
        eventually(async || !running(pid)).await,
        "program still running"
    );
    assert!(
        eventually(async || dir.sockets() == 0).await,
        "holder still there"
    );
}

#[tokio::test]
async fn a_program_that_exits_should_report_its_code_and_free_its_holder() {
    let dir = TestDir::new();
    let registry = dir.registry();
    registry
        .spawn(request("done", &["/bin/sh", "-c", "exit 3"]))
        .expect("spawns");
    assert!(eventually(async || !registry.info("done").expect("session").alive).await);
    assert_eq!(registry.info("done").expect("session").exit_code, Some(3));
    assert!(
        eventually(async || dir.sockets() == 0).await,
        "holder still there"
    );
}

fn running(pid: i32) -> bool {
    let status = Command::new("/bin/ps")
        .args(["-o", "stat=", "-p", &pid.to_string()])
        .output()
        .expect("ps");
    let state = String::from_utf8_lossy(&status.stdout);
    !state.trim().is_empty() && !state.trim().starts_with('Z')
}

/// The real core, on a port of its own, in a folder of its own.
struct Core {
    dir: PathBuf,
    process: Child,
    endpoint: String,
    token: String,
    http: reqwest::Client,
}

impl Core {
    async fn start(dir: &TestDir) -> Self {
        Self::launch(dir, Command::new(BINARY)).await
    }

    async fn launch(dir: &TestDir, mut command: Command) -> Self {
        let endpoint = dir.0.join("core").join("endpoint");
        let _ = std::fs::remove_file(&endpoint);
        command
            .env("TUI_DO_CORE_DIR", dir.0.join("core"))
            .env("TUI_DO_CORE_ADDR", "127.0.0.1:0")
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let process = loop {
            match command.spawn() {
                // A binary just written by this test, while another was forking
                Err(err) if err.kind() == std::io::ErrorKind::ExecutableFileBusy => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                result => break result.expect("starts the core"),
            }
        };
        let http = reqwest::Client::new();
        assert!(eventually(async || endpoint.exists()).await, "no endpoint");
        let address = std::fs::read_to_string(&endpoint).expect("endpoint");
        let up = eventually(async || {
            http.get(format!("{address}/v0/health"))
                .send()
                .await
                .is_ok()
        })
        .await;
        assert!(up, "the core didn't come up");
        let token = std::fs::read_to_string(dir.0.join("core").join("token")).expect("token");
        Self {
            dir: dir.0.join("core"),
            process,
            endpoint: address,
            token,
            http,
        }
    }

    fn signal(&self, signal: &str) {
        let pid = self.process.id().to_string();
        Command::new("/bin/kill")
            .args([signal, &pid])
            .status()
            .expect("kill");
    }

    /// End the core and wait for it, and return what's still needed to restart it.
    fn wait(&mut self) {
        let _ = self.process.wait();
    }

    async fn post(&self, path: &str, body: serde_json::Value) -> serde_json::Value {
        let res = self
            .http
            .post(format!("{}{path}", self.endpoint))
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await
            .expect("post");
        assert!(res.status().is_success(), "{path}: {}", res.status());
        res.json().await.unwrap_or_default()
    }

    async fn get(&self, path: &str) -> Option<serde_json::Value> {
        let res = self
            .http
            .get(format!("{}{path}", self.endpoint))
            .bearer_auth(&self.token)
            .send()
            .await
            .ok()?;
        if !res.status().is_success() {
            return None;
        }
        res.json().await.ok()
    }

    async fn spawn(&self, id: &str, script: &str) -> serde_json::Value {
        self.post(
            "/v0/sessions",
            serde_json::json!({ "id": id, "argv": ["/bin/sh", "-c", script], "agent": "other", "labels": { "task": "t" } }),
        )
        .await
    }

    async fn shows(&self, id: &str, text: &str) -> bool {
        eventually(async || {
            self.get(&format!("/v0/sessions/{id}/snapshot"))
                .await
                .is_some_and(|s| s["data"].as_str().is_some_and(|d| d.contains(text)))
        })
        .await
    }

    async fn shows_now(&self, id: &str, text: &str) -> bool {
        self.get(&format!("/v0/sessions/{id}/snapshot"))
            .await
            .is_some_and(|s| s["data"].as_str().is_some_and(|d| d.contains(text)))
    }

    async fn session(&self, id: &str) -> Option<serde_json::Value> {
        self.get(&format!("/v0/sessions/{id}")).await
    }

    fn sockets(&self) -> usize {
        std::fs::read_dir(self.dir.join("holders")).map_or(0, |dir| dir.flatten().count())
    }
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

#[tokio::test]
async fn a_file_sent_to_a_session_should_be_saved_until_it_s_removed() {
    use base64::Engine;
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    core.spawn("answer", "sleep 30").await;
    let body = serde_json::json!({
        "fileName": "Photo 1.png",
        "dataBase64": base64::engine::general_purpose::STANDARD.encode(b"png"),
    });
    let saved = core.post("/v0/sessions/answer/files", body.clone()).await;
    let path = PathBuf::from(saved["path"].as_str().expect("path"));
    assert_eq!(path, core.dir.join("uploads/answer/Photo-1.png"));
    assert_eq!(std::fs::read(&path).expect("saved"), b"png");

    let unknown = core
        .http
        .post(format!("{}/v0/sessions/nobody/files", core.endpoint))
        .bearer_auth(&core.token)
        .json(&body)
        .send()
        .await
        .expect("post");
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);

    let removed = core
        .http
        .delete(format!("{}/v0/sessions/answer", core.endpoint))
        .bearer_auth(&core.token)
        .send()
        .await
        .expect("delete");
    assert!(removed.status().is_success());
    assert!(!path.exists());
}

/// An install from before versions sat side by side has a plain file where
/// the current link goes, so its first start replaces the file it's running
/// from. On Linux, that leaves the core with a deleted path as its own.
#[tokio::test]
async fn a_core_that_moved_into_the_versions_folder_should_use_the_link() {
    use std::os::unix::fs::PermissionsExt;

    let dir = TestDir::new();
    let installed = dir.0.join("bin").join("tui-do-core");
    std::fs::create_dir_all(installed.parent().expect("bin")).expect("bin");
    std::fs::copy(BINARY, &installed).expect("installs");
    let tools = dir.0.join("tools");
    std::fs::create_dir_all(&tools).expect("tools");
    let calls = dir.0.join("claude-calls");
    let claude = tools.join("claude");
    let script = format!("#!/bin/sh\necho \"$@\" >> {}\n", calls.display());
    std::fs::write(&claude, script).expect("fake claude");
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).expect("runnable");
    std::fs::copy(&claude, tools.join("codex")).expect("fake codex");

    let mut command = Command::new(&installed);
    command
        .env("INVOCATION_ID", "test") // what makes it count as the service
        .env("HOME", &dir.0)
        .env_remove("CODEX_HOME")
        .env("PATH", format!("{}:/usr/bin:/bin", tools.display()));
    let core = Core::launch(&dir, command).await;

    let link = std::fs::symlink_metadata(&installed).expect("link");
    assert!(link.is_symlink(), "the core didn't move");
    #[cfg(target_os = "linux")]
    {
        let own = std::fs::read_link(format!("/proc/{}/exe", core.process.id())).expect("exe");
        assert!(own.to_string_lossy().ends_with(" (deleted)"), "{own:?}");
    }

    core.spawn("moved", "echo started; cat").await;
    assert!(core.shows("moved", "started").await);

    let setup = core
        .http
        .put(format!("{}/v0/agents/setup", core.endpoint))
        .bearer_auth(&core.token)
        .json(&serde_json::json!({ "instructions": "## Test" }))
        .send()
        .await
        .expect("put");
    assert!(setup.status().is_success(), "{}", setup.status());
    let done: serde_json::Value = setup.json().await.expect("json");
    let calls = std::fs::read_to_string(&calls).expect("claude was called");
    let registered = format!("tui-do -- {} mcp", installed.display());
    assert!(calls.contains(&registered), "{calls}");

    // Codex gets the hook its sessions report with, to review before it runs
    assert_eq!(done["codexHookToReview"], true);
    let hooks = std::fs::read_to_string(dir.0.join(".codex/hooks.json")).expect("Codex's hooks");
    let hooks: serde_json::Value = serde_json::from_str(&hooks).expect("json");
    let command = hooks["hooks"]["Stop"][0]["hooks"][0]["command"]
        .as_str()
        .expect("a command");
    assert!(
        command.ends_with(&format!("'{}' hook; true", installed.display())),
        "{command}"
    );
}

async fn survives(stop: &str) {
    let dir = TestDir::new();
    let mut core = Core::start(&dir).await;
    let spawned = core
        .spawn("keep", "echo first-line; printf '\\033[1mbold'; cat")
        .await;
    assert!(core.shows("keep", "first-line").await);

    core.signal(stop);
    core.wait();
    let next = Core::start(&dir).await;

    let session = next.session("keep").await.expect("session picked up");
    assert_eq!(session["alive"], true);
    assert_eq!(session["pid"], spawned["pid"]);
    assert_eq!(session["agent"], "other");
    assert_eq!(session["labels"]["task"], "t");
    assert!(
        next.shows("keep", "first-line").await,
        "screen not restored"
    );

    // It's the same program, still running and still listening
    next.post(
        "/v0/sessions/keep/input",
        serde_json::json!({ "data": "typed-after\n" }),
    )
    .await;
    assert!(next.shows("keep", "typed-after").await);
}

#[tokio::test]
async fn sessions_should_survive_the_core_stopping() {
    survives("-TERM").await;
}

#[tokio::test]
async fn sessions_should_survive_the_core_crashing() {
    survives("-KILL").await;
}

#[tokio::test]
async fn a_clean_stop_should_keep_what_the_output_buffer_has_lost() {
    let dir = TestDir::new();
    let mut core = Core::start(&dir).await;
    // A mode and a line from the start, then more output than a holder keeps
    // (escape codes, which don't use up the scrollback)
    let script = "printf '\\033[?2004h'; echo marker-at-start; \
        yes \"$(printf '\\033[0m')\" | head -n 600000 | tr -d '\\n'; \
        echo end-of-output; cat";
    core.spawn("long", script).await;
    let finished = eventually(async || {
        tokio::time::sleep(Duration::from_millis(200)).await;
        core.shows_now("long", "end-of-output").await
    })
    .await;
    assert!(finished, "the output didn't finish");

    core.signal("-TERM");
    core.wait();
    let next = Core::start(&dir).await;
    assert!(
        next.shows("long", "marker-at-start").await,
        "scrollback lost"
    );
    let screen = next
        .get("/v0/sessions/long/snapshot")
        .await
        .expect("snapshot");
    assert!(
        screen["data"]
            .as_str()
            .expect("data")
            .contains("\x1b[?2004h"),
        "mode lost"
    );
}

#[tokio::test]
async fn output_while_the_core_is_down_should_be_on_the_screen_afterwards() {
    let dir = TestDir::new();
    let mut core = Core::start(&dir).await;
    core.spawn("gap", "echo before; sleep 1; echo during-the-gap; sleep 30")
        .await;
    assert!(core.shows("gap", "before").await);
    core.signal("-KILL");
    core.wait();
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let next = Core::start(&dir).await;
    assert!(next.shows("gap", "before").await);
    assert!(next.shows("gap", "during-the-gap").await);
}

#[tokio::test]
async fn a_program_ending_while_the_core_is_down_should_show_as_exited() {
    let dir = TestDir::new();
    let mut core = Core::start(&dir).await;
    core.spawn("short", "echo bye-bye; sleep 1; exit 7").await;
    assert!(core.shows("short", "bye-bye").await);
    core.signal("-KILL");
    core.wait();
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let next = Core::start(&dir).await;
    let exited = eventually(async || {
        next.session("short")
            .await
            .is_some_and(|s| s["alive"] == false)
    })
    .await;
    assert!(exited, "still shown as running");
    assert_eq!(next.session("short").await.expect("session")["exitCode"], 7);
    assert!(next.shows("short", "bye-bye").await);
    assert!(
        eventually(async || next.sockets() == 0).await,
        "holder still there"
    );
}

#[tokio::test]
async fn a_holder_killed_should_only_end_its_own_session() {
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    core.spawn("victim", "sleep 30").await;
    core.spawn("other", "sleep 30").await;

    let holders = Command::new("/usr/bin/pgrep")
        .args([
            "-f",
            &format!("hold {}", dir.0.join("core/holders").display()),
        ])
        .output()
        .expect("pgrep");
    let holders: Vec<String> = String::from_utf8_lossy(&holders.stdout)
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(holders.len(), 2);
    Command::new("/bin/kill")
        .args(["-KILL", &holders[0]])
        .status()
        .expect("kill");

    let ended = eventually(async || {
        let (a, b) = (
            core.session("victim").await.expect("victim"),
            core.session("other").await.expect("other"),
        );
        (a["alive"] == false) != (b["alive"] == false)
    })
    .await;
    assert!(ended, "exactly one session should have ended");
}

#[tokio::test]
async fn a_stale_socket_should_be_cleaned_up_on_start() {
    let dir = TestDir::new();
    let holders = dir.0.join("core/holders");
    std::fs::create_dir_all(&holders).expect("folder");
    let stale = holders.join("stale.sock");
    drop(std::os::unix::net::UnixListener::bind(&stale).expect("binds"));
    assert!(stale.exists());

    let core = Core::start(&dir).await;
    assert!(!stale.exists());
    assert_eq!(core.sockets(), 0);
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

impl Core {
    async fn socket(&self, path: &str) -> Socket {
        let url = format!(
            "{}{path}token={}",
            self.endpoint.replacen("http", "ws", 1),
            self.token
        );
        tokio_tungstenite::connect_async(url.as_str())
            .await
            .expect("connects")
            .0
    }
}

/// The next JSON message, or `None` if nothing arrives in time.
async fn message(socket: &mut Socket, wait: Duration) -> Option<serde_json::Value> {
    loop {
        match tokio::time::timeout(wait, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => return serde_json::from_str(text.as_str()).ok(),
            Ok(Some(Ok(_))) => {}
            _ => return None,
        }
    }
}

#[tokio::test]
async fn a_paced_viewer_should_only_get_what_it_acknowledges_then_catch_up() {
    // Well past the window, but small: a debug build parses output slowly,
    // and a repaint waits for what the session hasn't parsed yet
    const LINES: usize = 5_000;
    const WINDOW: usize = 64 * 1024;
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    core.spawn(
        "paced",
        &format!(
            "i=0; while [ $i -lt {LINES} ]; do \
             echo line-$i-xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx; i=$((i+1)); done; \
             echo finished; sleep 60"
        ),
    )
    .await;
    let mut viewer = core.socket("/v0/sessions/paced/attach?paced=1&").await;
    let first = message(&mut viewer, Duration::from_secs(30))
        .await
        .expect("a snapshot");
    assert_eq!(first["type"], "snapshot");
    let mut unacknowledged = first["data"].as_str().expect("data").len();

    // Without acknowledgements the output stops at the window, and what's
    // held back gets old enough to be given up on
    let mut frames = 1;
    loop {
        let quiet = if unacknowledged < WINDOW { 30 } else { 3 };
        let Some(event) = message(&mut viewer, Duration::from_secs(quiet)).await else {
            break;
        };
        if event["type"] == "output" {
            unacknowledged += event["data"].as_str().expect("data").len();
            frames += 1;
        }
    }
    assert!(unacknowledged >= WINDOW, "only {unacknowledged} bytes came");
    assert!(
        unacknowledged < WINDOW + 16 * 1024,
        "{unacknowledged} bytes went out unacknowledged"
    );

    // Now it gets the current screen instead of everything it missed
    let ack = || Message::text(r#"{"type":"ack"}"#);
    for _ in 0..frames {
        viewer.send(ack()).await.expect("sends");
    }
    let mut repainted = false;
    let finished = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let event = message(&mut viewer, Duration::from_secs(30))
                .await
                .expect("output keeps coming");
            if !matches!(event["type"].as_str(), Some("output" | "snapshot")) {
                continue;
            }
            viewer.send(ack()).await.expect("sends");
            let data = event["data"].as_str().unwrap_or_default();
            repainted |= data.contains("\x1b[!p");
            if data.contains("finished") {
                return;
            }
        }
    })
    .await;
    assert!(finished.is_ok(), "the viewer never caught up");
    assert!(repainted, "the viewer got everything instead of a repaint");
}

#[tokio::test]
async fn an_unpaced_viewer_should_still_get_every_line() {
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    let mut viewer = {
        core.spawn("plain", "sleep 1; i=0; while [ $i -lt 500 ]; do echo line-$i; i=$((i+1)); done; echo finished; sleep 60").await;
        core.socket("/v0/sessions/plain/attach?").await
    };
    let mut all = String::new();
    while !all.contains("finished") {
        let event = message(&mut viewer, Duration::from_secs(20))
            .await
            .expect("output keeps coming");
        all.push_str(event["data"].as_str().unwrap_or_default());
    }
    assert!(all.contains("line-0") && all.contains("line-499"));
}

#[tokio::test]
async fn output_first_should_send_only_a_session_s_first_output() {
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    let mut events = core.socket("/v0/events?output=first&").await;
    assert_eq!(
        message(&mut events, Duration::from_secs(10))
            .await
            .expect("the session list")["type"],
        "sessions"
    );
    core.spawn(
        "first",
        "echo one; sleep 0.5; echo two; sleep 0.5; echo three",
    )
    .await;
    let mut outputs = 0;
    while let Some(event) = message(&mut events, Duration::from_secs(10)).await {
        match event["type"].as_str() {
            Some("output") => outputs += 1,
            Some("sessionExited") => break,
            _ => {}
        }
    }
    assert_eq!(outputs, 1);
}

#[tokio::test]
async fn a_waiting_agent_should_say_what_it_asks_and_when_someone_answers() {
    use tui_do_core::session::{AgentState, Event};
    let dir = TestDir::new();
    let registry = dir.registry();
    let mut events = registry.subscribe();
    let mut req = request(
        "agent",
        &[
            "/bin/sh",
            "-c",
            // What Claude's waiting hooks do: save their input, then report
            r#"printf '{"message":"Which one?"}' > "$TUI_DO_CLAUDE_STATE_FILE.json"; printf waiting > "$TUI_DO_CLAUDE_STATE_FILE"; sleep 10"#,
        ],
    );
    req.agent = Some("claude".into());
    registry.spawn(req).expect("spawns");

    let mut waiting = None;
    let mut typed = 0;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while let Ok(Ok(event)) = tokio::time::timeout_at(deadline, events.recv()).await {
        match event {
            Event::AgentState {
                state: AgentState::Waiting,
                detail,
                ..
            } => {
                waiting = Some(detail);
                // Typing an answer, twice: said once
                registry.input("agent", b"a".to_vec());
                registry.input("agent", b"b".to_vec());
            }
            Event::SessionTyped { .. } => typed += 1,
            _ => {}
        }
        if typed > 0 {
            // Give a second one the chance to show up
            tokio::time::sleep(Duration::from_millis(300)).await;
            while let Ok(event) = events.try_recv() {
                typed += usize::from(matches!(event, Event::SessionTyped { .. }));
            }
            break;
        }
    }
    registry.kill_all();
    assert_eq!(waiting, Some(Some("Which one?".to_string())));
    assert_eq!(typed, 1);
}

/// An agent session that runs `script` under `/bin/sh`.
fn agent_request(id: &str, agent: &str, script: &str) -> SpawnRequest {
    let mut req = request(id, &["/bin/sh", "-c", script]);
    req.agent = Some(agent.into());
    req
}

#[tokio::test]
async fn a_state_change_should_arrive_while_the_agent_keeps_printing() {
    use tui_do_core::session::AgentState;
    let dir = TestDir::new();
    let registry = dir.registry();
    // A spinner: output far more often than the state is looked at
    let script = r#"(sleep 0.3; printf '{"message":"Go on?"}' > "$TUI_DO_CLAUDE_STATE_FILE.json"; printf waiting > "$TUI_DO_CLAUDE_STATE_FILE") & while :; do printf .; sleep 0.02; done"#;
    registry
        .spawn(agent_request("agent", "claude", script))
        .expect("spawns");

    let waiting = eventually(async || {
        registry
            .info("agent")
            .is_some_and(|s| s.agent_state == Some(AgentState::Waiting))
    })
    .await;
    let detail = registry.info("agent").and_then(|s| s.agent_detail);
    registry.kill_all();
    assert!(waiting, "the hook's state was never read");
    // A client that connects now still learns what it waits for
    assert_eq!(detail.as_deref(), Some("Go on?"));
}

#[tokio::test]
async fn what_a_terminal_sends_by_itself_should_not_start_a_codex_turn() {
    use tui_do_core::session::AgentState;
    let dir = TestDir::new();
    let registry = dir.registry();
    // Codex ended its turn, and repaints a while after the terminal gets focus
    let script =
        r#"printf waiting > "$TUI_DO_CLAUDE_STATE_FILE"; sleep 1.5; printf repainted; sleep 10"#;
    registry
        .spawn(agent_request("agent", "codex", script))
        .expect("spawns");
    let is_waiting = || {
        registry
            .info("agent")
            .is_some_and(|s| s.agent_state == Some(AgentState::Waiting))
    };
    assert!(eventually(async || is_waiting()).await);

    // Focus in, and an answer to a query: not typing
    registry.input("agent", b"\x1b[I\x1b[?62;22c".to_vec());
    assert!(shows(&registry, "agent", "repainted").await);
    let still_waiting = is_waiting();
    registry.kill_all();
    assert!(still_waiting, "a repaint was taken for work");
}

fn relayed_device(id: &str, address: String, token: String) -> tui_do_core::fleet::Device {
    tui_do_core::fleet::Device {
        id: id.into(),
        name: id.into(),
        address,
        token,
        platform: "test".into(),
        added_at: String::new(),
        ssh_host: None,
        mcp_token: String::new(),
    }
}

#[tokio::test]
async fn a_device_s_removed_session_should_be_relayed() {
    use tui_do_core::relay::Relays;
    use tui_do_core::session::Event;
    let dir = TestDir::new();
    let core = Core::start(&dir).await;
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let relays = Relays::new(tx);
    relays.start(relayed_device(
        "mac",
        core.endpoint.clone(),
        core.token.trim().to_string(),
    ));
    core.spawn("gone", "sleep 60").await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let res = core
        .http
        .delete(format!("{}/v0/sessions/gone", core.endpoint))
        .bearer_auth(&core.token)
        .send()
        .await
        .expect("delete");
    assert!(res.status().is_success());
    let found = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match rx.recv().await {
                Ok(Event::DeviceEvent { device, event }) if event["type"] == "sessionRemoved" => {
                    break (device, event);
                }
                Ok(_) => {}
                Err(err) => panic!("{err}"),
            }
        }
    })
    .await
    .expect("the removal is relayed");
    assert_eq!(found.0, "mac");
    assert_eq!(found.1["id"], "gone");
}

#[tokio::test]
async fn a_silent_device_should_go_offline() {
    use tui_do_core::relay::Relays;
    use tui_do_core::session::Event;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("binds");
    let address = format!("http://{}", listener.local_addr().expect("address"));
    // Accepts, then never reads again: a peer that vanished without closing
    let _server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accepts");
        let socket = tokio_tungstenite::accept_async(stream)
            .await
            .expect("upgrades");
        tokio::time::sleep(Duration::from_secs(3600)).await;
        drop(socket);
    });
    let (tx, mut rx) = tokio::sync::broadcast::channel(256);
    let relays = Relays::new(tx);
    relays.start(relayed_device("mac", address, "t".into()));
    let offline = tokio::time::timeout(Duration::from_secs(90), async {
        loop {
            if let Ok(Event::DeviceStatus { online: false, .. }) = rx.recv().await {
                break;
            }
        }
    })
    .await;
    assert!(
        offline.is_ok(),
        "the relay still thinks the device is there"
    );
}

/// A Claude session whose hooks have said: a prompt, a tool call, and a
/// permission prompt for it. Then it sits there, as Claude would.
const ASKS_PERMISSION: &str = r#"
hook() { printf '%s' "$1" | "$TUI_DO_CORE_EXE" hook; }
hook '{"hook_event_name":"UserPromptSubmit","session_id":"s1"}'
hook '{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"make"}}'
hook '{"hook_event_name":"PermissionRequest","session_id":"s1","tool_name":"Bash","tool_input":{"command":"make"}}'
cat
"#;

impl Core {
    async fn spawn_claude(&self, id: &str, script: &str) -> serde_json::Value {
        self.post(
            "/v0/sessions",
            serde_json::json!({ "id": id, "argv": ["/bin/sh", "-c", script], "agent": "claude" }),
        )
        .await
    }

    /// Wait for a session's status to be in this state, and return it.
    async fn status(&self, id: &str, state: &str) -> Option<serde_json::Value> {
        let mut status = None;
        eventually(async || {
            status = self.session(id).await.map(|s| s["status"].clone());
            status.as_ref().is_some_and(|s| s["state"] == state)
        })
        .await
        .then_some(status)
        .flatten()
    }
}

#[tokio::test]
async fn hooks_should_give_a_session_its_status_and_the_next_core_the_same() {
    let dir = TestDir::new();
    let mut core = Core::start(&dir).await;
    let spawned = core.spawn_claude("agent", ASKS_PERMISSION).await;
    assert_eq!(spawned["status"]["state"], "starting");

    let blocked = core.status("agent", "blocked").await.expect("blocked");
    assert_eq!(blocked["reason"], "permission");
    assert_eq!(blocked["detail"], "Allow Bash: make?");
    assert_eq!(blocked["source"], "hooks");
    // Said the older way too
    let session = core.session("agent").await.expect("session");
    assert_eq!(session["agentState"], "waiting");
    assert_eq!(session["agentDetail"], "Allow Bash: make?");

    core.signal("-TERM");
    core.wait();
    let next = Core::start(&dir).await;
    let again = next.session("agent").await.expect("session picked up")["status"].clone();
    for same in ["state", "reason", "detail", "since", "source"] {
        assert_eq!(again[same], blocked[same], "{same}");
    }
    // A client that has the status from before keeps the newer of the two
    assert!(again["seq"].as_u64() >= blocked["seq"].as_u64());
}

#[tokio::test]
async fn claude_s_own_status_should_correct_what_no_hook_reports() {
    let dir = TestDir::new();
    // Stands in for the Claude process, which the core checks is running
    let mut claude = Command::new("/bin/sleep")
        .arg("120")
        .spawn()
        .expect("sleep");
    let sessions = dir.0.join("claude").join("sessions");
    std::fs::create_dir_all(&sessions).expect("folder");
    let file = sessions.join(format!("{}.json", claude.id()));
    let pid = claude.id();
    let says = |status: &str, at: u128| {
        let entry = serde_json::json!({ "pid": pid, "sessionId": "s1", "status": status, "statusUpdatedAt": at as u64 });
        // As Claude writes it: whole, or not there
        let part = file.with_extension("tmp");
        std::fs::write(&part, entry.to_string()).expect("write");
        std::fs::rename(&part, &file).expect("rename");
    };
    let now = || {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock")
            .as_millis()
    };
    // Busy since long before the permission prompt: nothing to correct
    says("busy", 1);

    let mut command = Command::new(BINARY);
    command.env("CLAUDE_CONFIG_DIR", dir.0.join("claude"));
    let core = Core::launch(&dir, command).await;
    core.spawn_claude("agent", ASKS_PERMISSION).await;
    assert!(core.status("agent", "blocked").await.is_some());
    tokio::time::sleep(Duration::from_millis(2500)).await;
    let still = core.session("agent").await.expect("session");
    assert_eq!(
        still["status"]["state"], "blocked",
        "the hook was overruled"
    );

    // The prompt is answered: no hook says so, Claude's status does
    says("busy", now());
    let working = core.status("agent", "working").await.expect("working");
    assert_eq!(working["source"], "claude");

    // Esc: the turn is over without a Stop hook
    says("idle", now());
    let ready = core.status("agent", "ready").await.expect("ready");
    assert_eq!(ready["reason"], "interrupted");
    let session = core.session("agent").await.expect("session");
    assert_eq!(session["agentState"], "idle");

    // Claude is killed: no SessionEnd hook either
    claude.kill().expect("kill");
    claude.wait().expect("wait");
    assert!(core.status("agent", "ended").await.is_some());
    let session = core.session("agent").await.expect("session");
    assert_eq!(session["alive"], true, "its shell is still there");
}

#[tokio::test]
async fn codex_s_notice_should_end_a_turn_its_stop_hook_didn_t() {
    use tui_do_core::status::State;
    let dir = TestDir::new();
    let registry = dir.registry();
    // Only some of Codex's hooks are trusted: a tool call is reported, the
    // end of the turn isn't. Its `notify` command says that, as it always did
    let script = r#"
printf '%s' '{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"make"}}' | "$TUI_DO_CORE_EXE" hook
sleep 1.2
printf '{"last-assistant-message":"All set"}' > "$TUI_DO_CLAUDE_STATE_FILE.json"
printf waiting > "$TUI_DO_CLAUDE_STATE_FILE"
cat
"#;
    registry
        .spawn(agent_request("agent", "codex", script))
        .expect("spawns");
    let status = || registry.info("agent").and_then(|session| session.status);
    let working = eventually(async || status().is_some_and(|s| s.state == State::Working)).await;
    let ready = eventually(async || status().is_some_and(|s| s.state == State::Ready)).await;
    let ended = status();
    registry.kill_all();
    assert!(working, "the tool call wasn't reported");
    assert!(ready, "the turn never ended");
    let ended = ended.expect("a status");
    assert_eq!(ended.reason.as_deref(), Some("finished"));
    assert_eq!(ended.detail.as_deref(), Some("All set"));
}
