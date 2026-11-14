//! A paired client's push tokens for its widgets and Live Activities, as a
//! real core keeps them.

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
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl Core {
    async fn start() -> Self {
        let name = uuid::Uuid::new_v4().simple().to_string();
        // Short: socket paths are limited to 104 bytes and TMPDIR can be long
        let root = PathBuf::from("/tmp").join(format!("tdp-{}", &name[..8]));
        let core_dir = root.join(".tui-do/core");
        std::fs::create_dir_all(&core_dir).expect("creates the core folder");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("finds a free port")
            .port();
        std::fs::write(core_dir.join("token"), TOKEN).expect("writes the token");
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
        for _ in 0..150 {
            if core.request(TOKEN, "GET", "/v0/device", None).await.0 == 200 {
                return core;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("the core should start");
    }

    /// The status and the body (null when there's none).
    async fn request(
        &self,
        token: &str,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> (u16, Value) {
        let method = reqwest::Method::from_bytes(method.as_bytes()).expect("method");
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.address))
            .bearer_auth(token);
        if let Some(body) = body {
            request = request.json(&body);
        }
        let Ok(response) = request.send().await else {
            return (0, Value::Null);
        };
        let status = response.status().as_u16();
        let text = response.text().await.unwrap_or_default();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
}

fn token(token: &str) -> Value {
    json!({ "token": token, "environment": "development", "topic": "com.itsnwa.tuido" })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_should_keep_its_widget_and_live_activity_tokens() {
    let core = Core::start().await;
    let (status, paired) = core
        .request(
            TOKEN,
            "POST",
            "/v0/clients",
            Some(json!({ "name": "iPhone" })),
        )
        .await;
    assert_eq!(status, 200, "{paired}");
    let phone = paired["token"]
        .as_str()
        .expect("a client token")
        .to_string();

    // Only a paired client has tokens
    let (status, _) = core
        .request(
            TOKEN,
            "PUT",
            "/v0/notifications/widgets",
            Some(token("ab12")),
        )
        .await;
    assert_eq!(status, 400);

    let (status, settings) = core.request(&phone, "GET", "/v0/notifications", None).await;
    assert_eq!(status, 200);
    assert_eq!(settings["widgets"], false);
    assert_eq!(settings["liveActivities"], false);

    let (status, settings) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/widgets",
            Some(token("AB12")),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(settings["widgets"], true);
    let (status, _) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/widgets",
            Some(token("not hex")),
        )
        .await;
    assert_eq!(status, 400);

    // An activity's token needs Live Activities on
    let (status, _) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/live-activities/cube/fix",
            Some(json!({ "token": "cd34" })),
        )
        .await;
    assert_eq!(status, 409);
    let (status, _) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/live-activities",
            Some(json!({ "enabled": true })),
        )
        .await;
    assert_eq!(status, 400);
    let (status, settings) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/live-activities",
            Some(json!({ "enabled": true, "startToken": "ef56", "environment": "development", "topic": "com.itsnwa.tuido" })),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(settings["liveActivities"], true);
    // No agent runs for it: the activity is told to end, and not kept
    let (status, _) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/live-activities/cube/fix",
            Some(json!({ "token": "cd34" })),
        )
        .await;
    assert_eq!(status, 204);
    let (status, _) = core
        .request(
            &phone,
            "DELETE",
            "/v0/notifications/live-activities/cube/fix",
            None,
        )
        .await;
    assert_eq!(status, 204);

    let (_, clients) = core.request(TOKEN, "GET", "/v0/clients", None).await;
    assert_eq!(clients[0]["widgets"], true);
    assert_eq!(clients[0]["liveActivities"], true);
    let stored =
        std::fs::read_to_string(core.root.join(".tui-do/core/clients.json")).expect("clients.json");
    assert!(stored.contains("ef56") && !stored.contains("cd34"));

    let (_, settings) = core
        .request(&phone, "DELETE", "/v0/notifications/widgets", None)
        .await;
    assert_eq!(settings["widgets"], false);
    let (_, settings) = core
        .request(
            &phone,
            "PUT",
            "/v0/notifications/live-activities",
            Some(json!({ "enabled": false })),
        )
        .await;
    assert_eq!(settings["liveActivities"], false);
}
