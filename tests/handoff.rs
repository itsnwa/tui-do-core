//! Handing a task to another machine and completing it, run for real: one
//! core is the home and the device, and (through a fleet entry that points
//! back at itself) the code machine.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

const BINARY: &str = env!("CARGO_BIN_EXE_tui-do-core");
const TOKEN: &str = "test-token";

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .envs([
            ("GIT_AUTHOR_NAME", "Test"),
            ("GIT_AUTHOR_EMAIL", "test@localhost"),
            ("GIT_COMMITTER_NAME", "Test"),
            ("GIT_COMMITTER_EMAIL", "test@localhost"),
        ])
        .output()
        .expect("runs git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

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
        let root = std::env::temp_dir().join(format!("tdh-{}", &name[..8]));
        let core_dir = root.join(".tui-do/core");
        std::fs::create_dir_all(&core_dir).expect("creates the core folder");
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .and_then(|l| l.local_addr())
            .expect("finds a free port")
            .port();
        let address = format!("http://127.0.0.1:{port}");
        std::fs::write(core_dir.join("token"), TOKEN).expect("writes the token");
        let fleet = json!({
            "generation": 1,
            "codeDevice": "code-machine",
            "devices": [{
                "id": "code-machine",
                "name": "Code machine",
                "address": address,
                "token": TOKEN,
                "platform": "",
                "addedAt": "",
            }],
        });
        std::fs::write(core_dir.join("fleet.json"), fleet.to_string()).expect("writes the fleet");
        let process = Command::new(BINARY)
            .env("HOME", &root)
            .env("TUI_DO_CORE_DIR", &core_dir)
            .env("TUI_DO_CORE_ADDR", format!("127.0.0.1:{port}"))
            .env_remove("XPC_SERVICE_NAME")
            .env_remove("INVOCATION_ID")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("starts the core");
        let core = Self {
            process,
            root,
            address,
            client: reqwest::Client::new(),
        };
        assert!(
            eventually(async || core.request("GET", "/v0/device", None).await.is_ok()).await,
            "the core should start"
        );
        let workspace = core.root.join("workspace");
        std::fs::create_dir_all(&workspace).expect("creates the workspace");
        core.call(
            "PUT",
            "/v0/workspace",
            json!({ "path": workspace.to_string_lossy() }),
        )
        .await;
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

    async fn mcp(&self, name: &str, arguments: Value) {
        let answer = self
            .call(
                "POST",
                "/v0/mcp/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await;
        assert!(answer.get("error").is_none(), "{name}: {answer}");
    }
}

/// Poll for up to fifteen seconds.
async fn eventually(mut check: impl AsyncFnMut() -> bool) -> bool {
    for _ in 0..300 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

#[derive(Clone, Copy, Debug)]
enum Completion {
    Route,
    Patch,
    McpComplete,
    McpUpdate,
}

/// Whichever way a task is completed, the device gives the work back and
/// keeps nothing of the task but the project's cache.
#[tokio::test(flavor = "multi_thread")]
async fn completing_a_handed_over_task_should_clear_it_from_the_device() {
    let core = Core::start().await;
    let project_dir = core.root.join("project");
    std::fs::create_dir_all(&project_dir).expect("creates the project");
    git(&project_dir, &["init", "--quiet", "-b", "main"]);
    std::fs::write(project_dir.join("README"), "hello\n").expect("writes a file");
    git(&project_dir, &["add", "README"]);
    git(&project_dir, &["commit", "--quiet", "-m", "first"]);
    std::fs::write(project_dir.join("uncommitted.txt"), "draft\n").expect("writes a file");

    let device = core
        .request("GET", "/v0/device", None)
        .await
        .expect("the device answers");
    let device = device["id"].as_str().expect("a device id").to_string();
    let project = core
        .call(
            "POST",
            "/v0/projects",
            json!({ "title": "Project", "directory": project_dir.to_string_lossy() }),
        )
        .await;
    let project = project["slug"]
        .as_str()
        .expect("a project slug")
        .to_string();
    let cache = core.root.join(format!(".tui-do/work/{project}/repo.git"));

    use Completion::{McpComplete, McpUpdate, Patch, Route};
    for (n, (completion, include_uncommitted, commits)) in [
        (Route, true, false),
        (Route, false, true),
        (Route, true, true),
        (Patch, true, false),
        (McpComplete, true, true),
        (McpUpdate, true, false),
    ]
    .into_iter()
    .enumerate()
    {
        let case = format!("{completion:?}, uncommitted {include_uncommitted}, commits {commits}");
        let task = core
            .call(
                "POST",
                "/v0/tasks",
                json!({ "title": format!("Task {n}"), "project": project }),
            )
            .await;
        let slug = task["slug"].as_str().expect("a task slug").to_string();
        let id = task["id"].as_str().expect("a task id").to_string();
        let handoff = core
            .call(
                "POST",
                &format!("/v0/tasks/{slug}/handoff"),
                json!({ "device": device, "includeUncommitted": include_uncommitted }),
            )
            .await;
        let folder = PathBuf::from(handoff["path"].as_str().expect("a folder"));
        assert!(folder.join("README").exists(), "{case}");
        assert_eq!(
            folder.join("uncommitted.txt").exists(),
            include_uncommitted,
            "{case}"
        );
        let branch = format!("tui-do/{id}");
        if commits {
            std::fs::write(folder.join("work.txt"), "done\n").expect("writes a file");
            git(&folder, &["add", "work.txt"]);
            git(&folder, &["commit", "--quiet", "-m", "work"]);
        }

        match completion {
            Route => {
                let done = core
                    .call(
                        "POST",
                        &format!("/v0/tasks/{id}/complete"),
                        json!({ "summary": "Done" }),
                    )
                    .await;
                assert_eq!(done["status"], "completed", "{case}");
            }
            Patch => {
                core.call(
                    "PATCH",
                    &format!("/v0/tasks/{slug}"),
                    json!({ "state": "completed" }),
                )
                .await;
            }
            McpComplete => {
                core.mcp("complete_task", json!({ "task_id": id, "summary": "Done" }))
                    .await;
            }
            McpUpdate => {
                core.mcp("update_task", json!({ "slug": slug, "state": "completed" }))
                    .await;
            }
        }

        // The record goes last, once the work is back
        let record = core.root.join(format!(".tui-do/core/handoffs/{slug}.json"));
        assert!(
            eventually(async || !record.exists()).await,
            "{case}: the handoff record should go"
        );
        assert!(!folder.exists(), "{case}: the task's folder should go");
        let refs = git(&cache, &["for-each-ref", "--format=%(refname)"]);
        assert!(
            !refs.contains(&branch) && !refs.contains(&slug),
            "{case}: the branch and refs should go, have {refs}"
        );
        assert!(cache.exists(), "{case}: the cache should stay");
        let landed = git(&project_dir, &["branch", "--list", &branch]);
        assert_eq!(!landed.is_empty(), commits, "{case}");
        if commits {
            let files = git(&project_dir, &["ls-tree", "--name-only", &branch]);
            assert!(files.contains("work.txt"), "{case}: {files}");
        }
    }
    // The checkout was never touched
    assert_eq!(
        git(&project_dir, &["rev-parse", "--abbrev-ref", "HEAD"]),
        "main"
    );
    assert!(project_dir.join("uncommitted.txt").exists());
}
