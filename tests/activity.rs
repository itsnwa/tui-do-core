//! What a task shows, across two real cores: a home with the tasks, and a
//! machine in its fleet with the task's agent session.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_tui-do-core");
const TOKEN: &str = "test-token";

struct Core {
    process: Child,
    root: PathBuf,
    address: String,
    client: reqwest::Client,
}

impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.process.kill();
        let _ = self.process.wait();
        // Its sessions' holders outlive it
        let _ = Command::new("/usr/bin/pkill")
            .args(["-f", &format!("hold {}/", self.root.display())])
            .status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Core {
    /// A core in a folder of its own, with these machines in its fleet.
    async fn start(devices: Value) -> Self {
        let name = uuid::Uuid::new_v4().simple().to_string();
        // Short: socket paths are limited to 104 bytes and TMPDIR can be long
        let root = PathBuf::from("/tmp").join(format!("tda-{}", &name[..8]));
        let core_dir = root.join(".tui-do/core");
        std::fs::create_dir_all(&core_dir).expect("creates the core folder");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("finds a free port")
            .port();
        std::fs::write(core_dir.join("token"), TOKEN).expect("writes the token");
        let fleet = json!({ "generation": 1, "devices": devices });
        std::fs::write(core_dir.join("fleet.json"), fleet.to_string()).expect("writes the fleet");
        let process = Command::new(BINARY)
            .env("HOME", &root)
            .env("TUI_DO_CORE_DIR", &core_dir)
            .env("TUI_DO_CORE_ADDR", format!("127.0.0.1:{port}"))
            .env_remove("XPC_SERVICE_NAME")
            .env_remove("INVOCATION_ID")
            .env_remove("CLAUDE_CONFIG_DIR")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts the core");
        let core = Self {
            process,
            root,
            address: format!("http://127.0.0.1:{port}"),
            client: reqwest::Client::new(),
        };
        assert!(
            eventually(async || core.request("GET", "/v0/device", None).await.is_ok()).await,
            "the core should start"
        );
        core
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.address))
            .bearer_auth(TOKEN);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request.send().await.map_err(|e| e.to_string())?;
        let status = response.status();
        let text = response.text().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(format!("{path} answered {status}: {text}"));
        }
        serde_json::from_str(&text).map_err(|e| e.to_string())
    }

    async fn call(&self, method: &str, path: &str, body: Value) -> Value {
        self.request(method, path, Some(body))
            .await
            .expect("the request should work")
    }

    /// Wait for a task's activity to show this word, and return it.
    async fn shows(&self, task: &str, word: &str) -> Option<Value> {
        let mut activity = Value::Null;
        eventually(async || {
            let all = self.request("GET", "/v0/activity", None).await;
            activity = all.map(|all| all[task].clone()).unwrap_or_default();
            activity["state"] == word
        })
        .await
        .then_some(activity)
    }
}

/// Poll for up to fifteen seconds.
async fn eventually(mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..150 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    false
}

/// What Claude's hooks say up to an open permission prompt; then it sits there.
const ASKS_PERMISSION: &str = r#"
hook() { printf '%s' "$1" | "$TUI_DO_CORE_EXE" hook; }
hook '{"hook_event_name":"UserPromptSubmit","session_id":"s1"}'
hook '{"hook_event_name":"PreToolUse","session_id":"s1","tool_name":"Bash","tool_use_id":"t1","tool_input":{"command":"make"}}'
hook '{"hook_event_name":"PermissionRequest","session_id":"s1","tool_name":"Bash","tool_input":{"command":"make"}}'
cat
"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_task_should_show_what_its_agent_on_another_machine_needs() {
    let machine = Core::start(json!([])).await;
    let home = Core::start(json!([{
        "id": "machine",
        "name": "Machine",
        "address": machine.address,
        "token": TOKEN,
        "platform": "",
        "addedAt": "",
    }]))
    .await;
    let workspace = home.root.join("workspace");
    std::fs::create_dir_all(&workspace).expect("creates the workspace");
    let path = json!({ "path": workspace.to_string_lossy() });
    home.call("PUT", "/v0/workspace", path).await;

    let task = home
        .call("POST", "/v0/tasks", json!({ "title": "Build it" }))
        .await;
    let slug = task["slug"].as_str().expect("a slug").to_string();
    let fresh = home.shows(&slug, "not-started").await.expect("not started");
    assert!(fresh.get("device").is_none());

    // Its agent starts on the machine and asks for a permission
    let started = json!({ "state": "in-progress", "device": "machine" });
    home.call("PATCH", &format!("/v0/tasks/{slug}"), started)
        .await;
    assert!(home.shows(&slug, "paused").await.is_some());
    let session =
        json!({ "id": slug, "argv": ["/bin/sh", "-c", ASKS_PERMISSION], "agent": "claude" });
    machine.call("POST", "/v0/sessions", session).await;

    let asks = home.shows(&slug, "needs-you").await.expect("needs you");
    assert_eq!(asks["device"], "machine");
    assert_eq!(asks["agent"], "claude");
    assert_eq!(asks["reachable"], true);
    assert_eq!(asks["status"]["state"], "blocked");
    assert_eq!(asks["status"]["detail"], "Allow Bash: make?");
    assert!(asks["seq"].as_u64() > fresh["seq"].as_u64());

    // The home lists the machine's session without asking the machine
    let sessions = home
        .request("GET", "/v0/fleet/sessions", None)
        .await
        .expect("sessions");
    let listed = sessions.as_array().expect("a list");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0]["device"], "machine");
    assert_eq!(listed[0]["status"]["state"], "blocked");

    // The machine drops off: the task keeps what was last heard, marked so
    drop(machine);
    let lost = eventually(async || {
        let all = home.request("GET", "/v0/activity", None).await;
        all.is_ok_and(|all| all[&slug]["reachable"] == false)
    })
    .await;
    assert!(lost, "the machine going away wasn't noticed");
    let all = home
        .request("GET", "/v0/activity", None)
        .await
        .expect("activity");
    assert_eq!(all[&slug]["state"], "needs-you");
    let sessions = home
        .request("GET", "/v0/fleet/sessions", None)
        .await
        .expect("sessions");
    assert_eq!(sessions, json!([]));

    // Completing it is the last word
    let done = json!({ "state": "completed" });
    home.call("PATCH", &format!("/v0/tasks/{slug}"), done).await;
    assert!(home.shows(&slug, "done").await.is_some());
}
