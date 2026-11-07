//! Starting a coding agent on a task, for clients that shouldn't have to know
//! how: the agents' options on this machine (`GET /v0/agents`), and
//! `POST /v0/tasks/{ref}/agent`, which hands the task's code to the machine it
//! runs on, starts its session there and types the agent's command once the
//! shell is up.

use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::{AppState, error};
use crate::fleet::Device;
use crate::session::{Event, SpawnRequest};
use crate::workspace::Error;
use crate::workspace::tasks::UpdateTask;

/// How long to wait for the shell's first output before typing anyway.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Gives Codex the TUI-DO tools on another machine: that machine's own
/// `tui-do-core mcp`, which forwards to the home core. `$HOME` is expanded there.
const CODEX_MACHINE_TOOLS: &str = concat!(
    r#" -c 'mcp_servers.tui-do.command="sh"'"#,
    r#" -c 'mcp_servers.tui-do.args=["-c", "exec \"$HOME/.tui-do/bin/tui-do-core\" mcp"]'"#,
);

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/agents", get(options))
        .route("/v0/tasks/{reference}/agent", post(start))
}

async fn options() -> Response {
    Json(crate::agent_models::options().await).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgent {
    /// The fleet device to run on; absent (or this core's id) = this machine.
    #[serde(default)]
    device: Option<String>,
    /// `claude` (the default) or `codex`.
    #[serde(default)]
    agent: Option<String>,
    /// Claude's `--model` value (`default` = none), or a Codex model slug.
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    effort: Option<String>,
    /// Claude's `--permission-mode`.
    #[serde(default)]
    permission_mode: Option<String>,
    /// Skip permission prompts (and Codex's sandbox).
    #[serde(default)]
    yolo: bool,
    /// Take the project's uncommitted changes along to another machine.
    #[serde(default)]
    include_uncommitted: bool,
    #[serde(default = "default_cols")]
    cols: u16,
    #[serde(default = "default_rows")]
    rows: u16,
}

fn default_cols() -> u16 {
    80
}
fn default_rows() -> u16 {
    24
}

#[derive(Debug, PartialEq, Eq)]
enum Agent {
    Claude,
    Codex,
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

/// Effort levels and permission modes are single words.
fn valid_word(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| c.is_ascii_alphanumeric())
}

/// The command typed into the session's shell, as the Mac app builds it.
fn command(agent: &Agent, req: &StartAgent, slug: &str, task_id: &str, remote: bool) -> String {
    let model = req
        .model
        .as_deref()
        .filter(|m| !m.is_empty() && *m != "default");
    let effort = req.effort.as_deref().filter(|e| valid_word(e));
    let prompt = format!("\"Start working on task {task_id}\"");
    match agent {
        Agent::Claude => {
            let mut cmd = format!("claude --settings \"${}\"", crate::hooks::SETTINGS_ENV);
            if let Some(mode) = req.permission_mode.as_deref().filter(|m| valid_word(m)) {
                cmd += &format!(" --permission-mode {mode}");
            }
            if let Some(model) = model {
                cmd += &format!(" --model {}", shell_quote(model));
            }
            if let Some(effort) = effort {
                cmd += &format!(" --effort {effort}");
            }
            if req.yolo {
                cmd += " --dangerously-skip-permissions";
            }
            format!("{cmd} -n \"{slug}\" {prompt}")
        }
        Agent::Codex => {
            let mut cmd = String::from("codex");
            if remote {
                cmd += CODEX_MACHINE_TOOLS;
            }
            if let Some(model) = model {
                cmd += &format!(" --model {}", shell_quote(model));
            }
            if let Some(effort) = effort {
                cmd += &format!(
                    " -c {}",
                    shell_quote(&format!("model_reasoning_effort=\"{effort}\""))
                );
            }
            if req.yolo {
                cmd += " --dangerously-bypass-approvals-and-sandbox";
            }
            format!("{cmd} {prompt}")
        }
    }
}

/// Start the task's agent session on the machine picked for it. A session
/// that's already running is returned as it is (`started: false`).
async fn start(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(req): Json<StartAgent>,
) -> Response {
    let agent = match req.agent.as_deref() {
        None | Some("claude") => Agent::Claude,
        Some("codex") => Agent::Codex,
        Some(other) => return error(StatusCode::BAD_REQUEST, &format!("unknown agent {other:?}")),
    };
    if req.cols == 0 || req.rows == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "cols and rows must be greater than zero",
        );
    }
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let ws = std::sync::Arc::clone(&workspace);
    let found = tokio::task::spawn_blocking(move || {
        let slug = ws
            .find_task(&reference)?
            .ok_or_else(|| Error::NotFound(format!("Task not found: \"{reference}\"")))?;
        let task = ws.get_task(&slug)?;
        let directory = if task.task.project.is_empty() {
            String::new()
        } else {
            ws.get_project(&task.task.project)?.directory
        };
        Ok::<_, Error>((task.task, directory))
    })
    .await;
    let (task, directory) = match found {
        Ok(Ok(found)) => found,
        Ok(Err(Error::NotFound(msg))) => return error(StatusCode::NOT_FOUND, &msg),
        Ok(Err(err)) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };

    let own_id = state.config.get().device_id;
    let device = match req
        .device
        .as_deref()
        .filter(|d| !d.is_empty() && *d != own_id)
    {
        Some(id) => match state.fleet.device(id) {
            Some(device) => Some(device),
            None => return error(StatusCode::NOT_FOUND, "no such machine"),
        },
        None => None,
    };
    if device.is_none() && directory.is_empty() {
        return error(
            StatusCode::BAD_REQUEST,
            "the task's project has no folder to work in",
        );
    }

    if let Some(running) = running_session(&state, device.as_ref(), &task.slug).await {
        return Json(json!({
            "session": running,
            "device": device.as_ref().map_or(own_id.as_str(), |d| d.id.as_str()),
            "started": false,
        }))
        .into_response();
    }

    // Subscribe before the session exists, so its first output isn't missed
    let events = state.registry.subscribe();
    let (session, path) = match &device {
        Some(device) => {
            let path = match state
                .handoffs
                .start(workspace, &task.slug, device, req.include_uncommitted)
                .await
            {
                Ok((path, _branch)) => path,
                Err(err) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
            };
            match spawn_on_device(&state, device, &task.slug, &path, &agent, &req).await {
                Ok(session) => (session, path),
                Err(err) => return error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
            }
        }
        None => {
            let spawn = SpawnRequest {
                id: task.slug.clone(),
                argv: Vec::new(),
                cwd: Some(directory.clone()),
                env: Default::default(),
                cols: req.cols,
                rows: req.rows,
                agent: Some(agent_name(&agent).into()),
                labels: Default::default(),
            };
            let registry = std::sync::Arc::clone(&state.registry);
            match tokio::task::spawn_blocking(move || registry.spawn(spawn)).await {
                Ok(Ok(info)) => (serde_json::to_value(info).unwrap_or(Value::Null), directory),
                Ok(Err(err)) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
                Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
            }
        }
    };

    // The task remembers where it runs
    let device_id = device.as_ref().map(|d| d.id.clone());
    if task.device != device_id {
        let ws = state.workspace.current();
        let slug = task.slug.clone();
        let change = device_id.clone();
        let _ = tokio::task::spawn_blocking(move || {
            ws.map(|ws| {
                ws.update_task(
                    &slug,
                    UpdateTask {
                        device: Some(change),
                        ..Default::default()
                    },
                )
            })
        })
        .await;
    }

    let command = command(&agent, &req, &task.slug, &task.id, device.is_some());
    tokio::spawn(type_when_ready(
        state.clone(),
        device.clone(),
        task.slug.clone(),
        command,
        events,
    ));
    tracing::info!(task = %task.slug, device = ?device_id, agent = agent_name(&agent), "agent session started");
    Json(json!({
        "session": session,
        "device": device_id.unwrap_or(own_id),
        "path": path,
        "started": true,
    }))
    .into_response()
}

fn agent_name(agent: &Agent) -> &'static str {
    match agent {
        Agent::Claude => "claude",
        Agent::Codex => "codex",
    }
}

/// The task's live session on that machine, if there is one.
async fn running_session(state: &AppState, device: Option<&Device>, slug: &str) -> Option<Value> {
    let Some(device) = device else {
        return state
            .registry
            .info(slug)
            .filter(|s| s.alive)
            .and_then(|s| serde_json::to_value(s).ok());
    };
    let res = state
        .fleet
        .client()
        .get(format!("{}/v0/sessions/{slug}", device.address))
        .bearer_auth(&device.token)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let session: Value = res.error_for_status().ok()?.json().await.ok()?;
    (session["alive"] == true).then_some(session)
}

async fn spawn_on_device(
    state: &AppState,
    device: &Device,
    slug: &str,
    path: &str,
    agent: &Agent,
    req: &StartAgent,
) -> anyhow::Result<Value> {
    // The machine's own login shell and environment, in the task's folder there
    let res = state
        .fleet
        .client()
        .post(format!("{}/v0/sessions", device.address))
        .bearer_auth(&device.token)
        .json(&json!({
            "id": slug,
            "argv": [],
            "cwd": path,
            "cols": req.cols,
            "rows": req.rows,
            "agent": agent_name(agent),
        }))
        .send()
        .await
        .map_err(|err| anyhow::anyhow!("{} isn't reachable: {err}", device.name))?;
    if !res.status().is_success() {
        let body: Value = res.json().await.unwrap_or(Value::Null);
        anyhow::bail!(
            "{} couldn't start the session: {}",
            device.name,
            body["error"].as_str().unwrap_or("unknown error")
        );
    }
    Ok(res.json().await?)
}

/// Type the agent's command once the session's shell has printed something
/// (its prompt), or after a few seconds.
async fn type_when_ready(
    state: AppState,
    device: Option<Device>,
    slug: String,
    command: String,
    mut events: tokio::sync::broadcast::Receiver<Event>,
) {
    let ready = async {
        loop {
            match events.recv().await {
                Ok(Event::Output { id, .. }) if device.is_none() && *id == *slug => return,
                Ok(Event::DeviceEvent {
                    device: from,
                    event,
                }) if device.as_ref().is_some_and(|d| d.id == from)
                    && event["type"] == "output"
                    && event["id"] == slug.as_str() =>
                {
                    return;
                }
                Err(RecvError::Closed) => return,
                _ => {}
            }
        }
    };
    let _ = tokio::time::timeout(SHELL_READY_TIMEOUT, ready).await;
    let data = format!("{command}\n");
    match device {
        None => {
            state.registry.input(&slug, data.into_bytes());
        }
        Some(device) => {
            let sent = state
                .fleet
                .client()
                .post(format!("{}/v0/sessions/{slug}/input", device.address))
                .bearer_auth(&device.token)
                .json(&json!({ "data": data }))
                .send()
                .await
                .and_then(reqwest::Response::error_for_status);
            if let Err(err) = sent {
                tracing::warn!(task = %slug, device = %device.name, %err, "couldn't type the agent's command");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(
        model: Option<&str>,
        effort: Option<&str>,
        mode: Option<&str>,
        yolo: bool,
    ) -> StartAgent {
        StartAgent {
            device: None,
            agent: None,
            model: model.map(str::to_string),
            effort: effort.map(str::to_string),
            permission_mode: mode.map(str::to_string),
            yolo,
            include_uncommitted: false,
            cols: 80,
            rows: 24,
        }
    }

    #[test]
    fn claude_command_should_match_the_mac_apps() {
        let req = request(
            Some("claude-fable-5-1[1m]"),
            Some("high"),
            Some("plan"),
            true,
        );
        assert_eq!(
            command(&Agent::Claude, &req, "my-task", "26-001", false),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" --permission-mode plan \
             --model 'claude-fable-5-1[1m]' --effort high --dangerously-skip-permissions \
             -n \"my-task\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn default_model_should_add_no_flag() {
        let req = request(Some("default"), None, None, false);
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" -n \"t\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn codex_on_a_machine_should_get_the_tools_and_effort() {
        let req = request(Some("gpt-6"), Some("high"), None, false);
        let cmd = command(&Agent::Codex, &req, "t", "26-001", true);
        assert!(cmd.starts_with("codex -c 'mcp_servers.tui-do.command=\"sh\"'"));
        assert!(cmd.ends_with(
            " --model 'gpt-6' -c 'model_reasoning_effort=\"high\"' \"Start working on task 26-001\""
        ));
    }

    #[test]
    fn odd_effort_and_mode_values_should_be_dropped() {
        let req = request(None, Some("high; rm -rf ~"), Some("$(x)"), false);
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" -n \"t\" \"Start working on task 26-001\""
        );
    }
}
