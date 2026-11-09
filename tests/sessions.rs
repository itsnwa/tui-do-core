//! Terminal sessions, run for real: the actual binary as holder and core.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

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
            Hooks::prepare(&self.0.join("claude-hooks")).expect("hooks"),
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

#[test]
fn spawn_should_report_a_program_that_cannot_start() {
    let dir = TestDir::new();
    let registry = dir.registry();
    assert!(
        registry
            .spawn(request("missing", &["/no/such/program"]))
            .is_err()
    );
    assert_eq!(dir.sockets(), 0);
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

    let mut command = Command::new(&installed);
    command
        .env("INVOCATION_ID", "test") // what makes it count as the service
        .env("HOME", &dir.0)
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
    let calls = std::fs::read_to_string(&calls).expect("claude was called");
    let registered = format!("tui-do -- {} mcp", installed.display());
    assert!(calls.contains(&registered), "{calls}");
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
