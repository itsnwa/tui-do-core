//! The Assistant (see `crate::assistant`) on the home core: its session,
//! `GET`/`POST /v0/assistant`, and the tools only it gets, which its MCP
//! server sends to `POST /v0/mcp/call` marked as the Assistant's.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use rmcp::model::JsonObject;
use serde::Deserialize;
use serde_json::{Value, json};

use super::agent::{self, StartAgent, Worker};
use super::{AppState, error};
use crate::assistant::{self as assistant, SESSION_ID};
use crate::fleet::Device;
use crate::session::{Event, SpawnRequest};
use crate::workspace::projects::CreateProject;

/// How long a fresh start waits for the old session to end, so its exit
/// can't be taken for the new one's.
const ENDING_WAIT: Duration = Duration::from_secs(5);
/// Lines `read_session` gives by default, and at most.
const READ_LINES: usize = 80;
const MAX_READ_LINES: usize = 500;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/v0/assistant", get(status).post(start))
}

async fn status(State(state): State<AppState>) -> Response {
    let session = state.registry.info(SESSION_ID).filter(|s| s.alive);
    Json(json!({
        "session": session,
        "device": state.config.get().device_id,
    }))
    .into_response()
}

#[derive(Deserialize)]
#[serde(default)]
struct Start {
    /// End the running Assistant and start a new conversation.
    fresh: bool,
    cols: u16,
    rows: u16,
}

impl Default for Start {
    fn default() -> Self {
        Self {
            fresh: false,
            cols: 120,
            rows: 40,
        }
    }
}

/// Start the Assistant, or answer with the one that's running
/// (`started: false`).
async fn start(State(state): State<AppState>, Json(req): Json<Start>) -> Response {
    let config = state.config.get();
    if let Some(home) = &config.home {
        return (
            StatusCode::CONFLICT,
            Json(json!({
                "error": format!("The Assistant runs on the home, {}.", home.name),
                "home": super::home::home_summary(&state),
            })),
        )
            .into_response();
    }
    if req.cols == 0 || req.rows == 0 {
        return error(
            StatusCode::BAD_REQUEST,
            "cols and rows must be greater than zero",
        );
    }
    let running = state.registry.info(SESSION_ID).filter(|s| s.alive);
    if let Some(session) = running.as_ref().filter(|_| !req.fresh) {
        return Json(json!({
            "session": session,
            "device": config.device_id,
            "started": false,
        }))
        .into_response();
    }
    if running.is_some() {
        end(&state).await;
    }

    let folder = assistant::folder(&state.core_dir);
    let home_name = config.device_name.clone();
    let prepared = {
        let folder = folder.clone();
        tokio::task::spawn_blocking(move || {
            assistant::prepare(&folder, &home_name)?;
            Ok::<_, anyhow::Error>(assistant::has_conversation(&folder))
        })
        .await
    };
    let earlier = match prepared {
        Ok(Ok(earlier)) => earlier,
        Ok(Err(err)) => return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // Subscribe before the session exists, so its first output isn't missed
    let events = state.registry.subscribe();
    let spawn = SpawnRequest {
        id: SESSION_ID.into(),
        argv: Vec::new(),
        cwd: Some(folder.display().to_string()),
        env: Default::default(),
        cols: req.cols,
        rows: req.rows,
        agent: Some("claude".into()),
        labels: [(assistant::ROLE_LABEL.into(), assistant::ROLE.into())].into(),
    };
    let session = match agent::spawn_session(&state, None, spawn).await {
        Ok(session) => session,
        Err(err) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    };
    let command = assistant::command(earlier && !req.fresh);
    tokio::spawn(agent::type_when_ready(
        state.clone(),
        None,
        SESSION_ID.into(),
        command,
        events,
    ));
    tracing::info!(fresh = req.fresh, "assistant started");
    Json(json!({
        "session": session,
        "device": config.device_id,
        "started": true,
    }))
    .into_response()
}

/// End the running Assistant, and wait until it has.
async fn end(state: &AppState) {
    let mut events = state.registry.subscribe();
    state.registry.remove(SESSION_ID);
    let ended = async {
        loop {
            match events.recv().await {
                Ok(Event::SessionExited { id, .. }) if &*id == SESSION_ID => return,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                _ => {}
            }
        }
    };
    if tokio::time::timeout(ENDING_WAIT, ended).await.is_err() {
        tracing::warn!("the old assistant session didn't say it ended");
    }
}

/// Run one of the Assistant's tools. Only on the home: the Assistant runs
/// there, and the tools reach the other machines from it.
pub(super) async fn call_tool(
    state: &AppState,
    name: &str,
    args: JsonObject,
) -> crate::mcp::CallOutcome {
    use crate::mcp::CallOutcome;
    if !state.config.get().is_home() {
        return CallOutcome::Error(
            "These tools run on the TUI-DO home, and this machine isn't it.".into(),
        );
    }
    let args = Args(args);
    let result = match name {
        "list_machines" => Ok(list_machines(state).await),
        "list_sessions" => list_sessions(state, &args).await,
        "read_session" => read_session(state, &args).await,
        "start_agent" => start_agent(state, &args).await,
        "start_session" => start_session(state, &args).await,
        "list_folders" => list_folders(state, &args).await,
        "set_up_project" => set_up_project(state, &args).await,
        other => Err(format!("Unknown tool: {other}")),
    };
    match result {
        Ok(value) => CallOutcome::Result(value),
        Err(message) => CallOutcome::Error(message),
    }
}

struct Args(JsonObject);

impl Args {
    fn opt(&self, key: &str) -> Option<String> {
        self.0
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    }

    fn req(&self, key: &str) -> Result<String, String> {
        self.opt(key)
            .ok_or_else(|| format!("Invalid arguments: \"{key}\" is required and must be a string"))
    }
}

/// The machine a tool names, by id or name, or `default` (an id).
fn machine(state: &AppState, given: Option<&str>, default: &str) -> Result<Device, String> {
    let config = state.config.get();
    let id = match given {
        None => default.to_string(),
        Some(g) if g.eq_ignore_ascii_case(&config.device_id) => config.device_id.clone(),
        Some(g) if g.eq_ignore_ascii_case(&config.device_name) => config.device_id.clone(),
        Some(g) => state
            .fleet
            .devices()
            .into_iter()
            .find(|d| d.id.eq_ignore_ascii_case(g) || d.name.eq_ignore_ascii_case(g))
            .map(|d| d.id)
            .ok_or_else(|| format!("There's no machine {g:?}; list_machines has them."))?,
    };
    super::fleet::target(state, &id).ok_or_else(|| format!("There's no machine {id:?}."))
}

async fn list_machines(state: &AppState) -> Value {
    let config = state.config.get();
    let others: Vec<Value> = state
        .fleet
        .statuses()
        .await
        .into_iter()
        .map(|d| {
            json!({
                "id": d.id,
                "name": d.name,
                "platform": d.platform,
                "online": d.online,
                "version": d.version,
            })
        })
        .collect();
    json!({
        "home": {
            "id": config.device_id,
            "name": config.device_name,
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "note": "The home holds the tasks, and you run on it.",
        },
        "projectsMachine": state.fleet.code_device(&config.device_id),
        "machines": others,
    })
}

async fn list_sessions(state: &AppState, args: &Args) -> Result<Value, String> {
    let config = state.config.get();
    let only = match args.opt("machine") {
        Some(given) => Some(machine(state, Some(&given), &config.device_id)?.id),
        None => None,
    };
    let mut sessions: Vec<Value> = state
        .registry
        .list()
        .into_iter()
        .filter(|s| s.id != SESSION_ID)
        .filter_map(|s| serde_json::to_value(s).ok())
        .map(|mut s| {
            s["device"] = config.device_id.clone().into();
            s
        })
        .collect();
    sessions.extend(state.activity.fleet_sessions());
    sessions.retain(|s| s["alive"] == true && only.as_deref().is_none_or(|id| s["device"] == id));
    let names: std::collections::HashMap<String, String> = state
        .fleet
        .devices()
        .into_iter()
        .map(|d| (d.id, d.name))
        .chain([(config.device_id.clone(), config.device_name.clone())])
        .collect();
    let ws = state.workspace.current();
    let listed = tokio::task::spawn_blocking(move || {
        sessions
            .into_iter()
            .map(|s| {
                let id = s["id"].as_str().unwrap_or_default();
                let device = s["device"].as_str().unwrap_or_default();
                let task = ws.as_ref().and_then(|ws| ws.get_task(id).ok()).map(
                    |t| json!({ "id": t.task.id, "title": t.task.title, "state": t.task.state }),
                );
                json!({
                    "session": id,
                    "machine": names.get(device).map_or(device, String::as_str),
                    "machineId": device,
                    "task": task,
                    "title": s["labels"][assistant::TITLE_LABEL],
                    "agent": s["agent"],
                    "agentState": s["agentState"],
                    "asks": s["agentDetail"],
                })
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| e.to_string())?;
    Ok(Value::Array(listed))
}

async fn read_session(state: &AppState, args: &Args) -> Result<Value, String> {
    let id = args.req("session")?;
    let own_id = state.config.get().device_id;
    let target = machine(state, args.opt("machine").as_deref(), &own_id)?;
    let lines = args
        .0
        .get("lines")
        .and_then(Value::as_u64)
        .map_or(READ_LINES, |n| (n as usize).clamp(1, MAX_READ_LINES));
    let (data, session) = if target.id == own_id {
        let snapshot = state
            .registry
            .snapshot(&id)
            .await
            .ok_or_else(|| format!("There's no session {id:?} on {}.", target.name))?;
        let session = serde_json::to_value(snapshot.info).map_err(|e| e.to_string())?;
        (snapshot.data, session)
    } else {
        let res = state
            .fleet
            .client()
            .get(format!("{}/v0/sessions/{id}/snapshot", target.address))
            .bearer_auth(&target.token)
            .timeout(Duration::from_secs(10))
            .send()
            .await
            .map_err(|e| format!("{} isn't reachable: {e}", target.name))?;
        if res.status() == StatusCode::NOT_FOUND {
            return Err(format!("There's no session {id:?} on {}.", target.name));
        }
        let mut body: Value = res
            .error_for_status()
            .map_err(|e| e.to_string())?
            .json()
            .await
            .map_err(|e| e.to_string())?;
        let data = body["data"].as_str().unwrap_or_default().to_string();
        (data, body["session"].take())
    };
    let size = |key: &str| {
        session[key]
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0)
    };
    let (cols, rows) = (size("cols").unwrap_or(80), size("rows").unwrap_or(24));
    let screen =
        tokio::task::spawn_blocking(move || crate::session::screen_text(&data, cols, rows, lines))
            .await
            .map_err(|e| e.to_string())?;
    Ok(json!({
        "session": id,
        "machine": target.name,
        "alive": session["alive"],
        "agentState": session["agentState"],
        "asks": session["agentDetail"],
        "screen": screen,
    }))
}

fn agent_request(args: &Args, device: Option<String>) -> Result<StartAgent, String> {
    let permission_mode =
        crate::mcp::tools::agent_permission_mode(args.opt("permission_mode").as_deref())?;
    Ok(StartAgent::for_assistant(
        device,
        args.opt("agent"),
        args.opt("model"),
        args.opt("effort"),
        permission_mode,
    ))
}

async fn start_agent(state: &AppState, args: &Args) -> Result<Value, String> {
    let task = args.req("task_id")?;
    let device = match args.opt("machine") {
        Some(given) => Some(machine(state, Some(&given), "")?.id),
        None => None,
    };
    let req = agent_request(args, device)?;
    let mut started = agent::start_agent(state, task, req, None)
        .await
        .map_err(|(_, message)| message)?;
    if let Some(device) = started["device"].as_str() {
        started["machine"] =
            machine(state, Some(device), device).map_or_else(|_| device.into(), |d| d.name.into());
    }
    Ok(started)
}

async fn start_session(state: &AppState, args: &Args) -> Result<Value, String> {
    let own_id = state.config.get().device_id;
    let target = machine(state, args.opt("machine").as_deref(), &own_id)?;
    let worker = Worker {
        target,
        name: args.req("name")?,
        prompt: args.req("prompt")?,
        folder: args.opt("folder").unwrap_or_else(|| "~".into()),
        req: agent_request(args, None)?,
    };
    agent::start_worker(state, worker)
        .await
        .map_err(|(_, message)| message)
}

async fn list_folders(state: &AppState, args: &Args) -> Result<Value, String> {
    let own_id = state.config.get().device_id;
    let target = machine(state, args.opt("machine").as_deref(), &own_id)?;
    let path = args.opt("path").unwrap_or_else(|| "~".into());
    let res = state
        .fleet
        .client()
        .get(format!("{}/v0/fs/dirs", target.address))
        .query(&[("path", path)])
        .bearer_auth(&target.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| format!("{} isn't reachable: {e}", target.name))?;
    answer(&target, res).await
}

/// A machine's answer: its JSON, or its error as the tool's.
async fn answer(target: &Device, res: reqwest::Response) -> Result<Value, String> {
    let status = res.status();
    let body: Value = res.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(body);
    }
    if status == StatusCode::NOT_FOUND && body["error"].is_null() {
        return Err(format!(
            "{} doesn't know that yet; update it to the home's version in TUI-DO's settings.",
            target.name
        ));
    }
    Err(format!(
        "{}: {}",
        target.name,
        body["error"].as_str().unwrap_or("unknown error")
    ))
}

async fn set_up_project(state: &AppState, args: &Args) -> Result<Value, String> {
    let title = args.req("title")?;
    let folder = args.req("folder")?;
    let own_id = state.config.get().device_id;
    let code = state.fleet.code_device(&own_id);
    let target = machine(state, args.opt("machine").as_deref(), &code)?;
    // A git clone or a push to GitHub can take a while
    let res = state
        .fleet
        .client()
        .post(format!("{}/v0/fs/repo", target.address))
        .bearer_auth(&target.token)
        .timeout(Duration::from_secs(300))
        .json(&json!({
            "path": folder,
            "clone": args.opt("clone"),
            "github": args.opt("github"),
        }))
        .send()
        .await
        .map_err(|e| format!("{} isn't reachable: {e}", target.name))?;
    let repo = answer(&target, res).await?;
    let directory = repo["path"].as_str().unwrap_or(&folder).to_string();
    let Some(ws) = state.workspace.current() else {
        return Err("Workspace not configured.".into());
    };
    let create = CreateProject {
        title,
        // The projects' machine is the default a project has without one
        machine: (target.id != code).then(|| target.id.clone()),
        directory: Some(directory),
        remote: repo["remote"].as_str().map(str::to_string),
    };
    // Held until the project is written, so a move of the home waits for it
    let _writing = state.workspace.writing().await;
    if state.workspace.is_frozen() {
        return Err("The TUI-DO home is moving to another machine; try again in a moment.".into());
    }
    let project = tokio::task::spawn_blocking(move || ws.create_project(create))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "project": project,
        "machine": target.name,
        "repoCreated": repo["created"],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_start_without_a_size_should_get_a_roomy_one() {
        let start: Start = serde_json::from_value(json!({})).expect("start");
        assert!(!start.fresh);
        assert_eq!((start.cols, start.rows), (120, 40));
        let fresh: Start = serde_json::from_value(json!({ "fresh": true })).expect("fresh");
        assert!(fresh.fresh);
    }

    #[test]
    fn arguments_should_be_trimmed_and_blank_ones_left_out() {
        let Value::Object(map) = json!({ "machine": " cube ", "folder": "  " }) else {
            unreachable!()
        };
        let args = Args(map);
        assert_eq!(args.opt("machine").as_deref(), Some("cube"));
        assert_eq!(args.opt("folder"), None);
        assert!(args.req("folder").is_err());
    }
}
