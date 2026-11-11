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
use crate::links::SessionLink;
use crate::session::{Event, SpawnRequest};
use crate::workspace::Error;
use crate::workspace::tasks::{Task, UpdateTask};

/// How long to wait for the shell's first output before typing anyway.
const SHELL_READY_TIMEOUT: Duration = Duration::from_secs(5);

/// Gives Codex the TUI-DO tools on another machine: that machine's own
/// `tui-do-core mcp`, which forwards to the home core. `$HOME` is expanded there.
const CODEX_MACHINE_TOOLS: &str = concat!(
    r#" -c 'mcp_servers.tui-do.command="sh"'"#,
    r#" -c 'mcp_servers.tui-do.args=["-c", "exec \"$HOME/.tui-do/bin/tui-do-core\" mcp"]'"#,
);

/// Reports each finished turn to `tui-do-core codex-notify`, which Codex runs
/// with the turn's details as the last argument. `$TUI_DO_CORE_EXE` comes from
/// the session.
const CODEX_NOTIFY: &str =
    r#" -c 'notify=["sh","-c","exec \"$TUI_DO_CORE_EXE\" codex-notify \"$1\"","sh"]'"#;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/agents", get(options))
        .route("/v0/tasks/{reference}/agent", post(start))
        .route("/v0/tasks/{slug}/codex-session", get(codex_session))
}

async fn options() -> Response {
    Json(crate::agent_models::options().await).into_response()
}

/// The Codex session this machine has for the task, if it can still be resumed.
fn local_codex_link(state: &AppState, slug: &str) -> Option<SessionLink> {
    let link = state.codex_sessions.get(slug)?;
    std::path::Path::new(&link.cwd).is_dir().then_some(link)
}

async fn codex_session(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    match local_codex_link(&state, &slug) {
        Some(link) => Json(link).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "no Codex session linked to this task on this machine",
        ),
    }
}

/// The task's Codex session on the machine it would run on.
async fn codex_link(state: &AppState, device: Option<&Device>, slug: &str) -> Option<SessionLink> {
    let Some(device) = device else {
        return local_codex_link(state, slug);
    };
    let res = state
        .fleet
        .client()
        .get(format!("{}/v0/tasks/{slug}/codex-session", device.address))
        .bearer_auth(&device.token)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    res.error_for_status().ok()?.json().await.ok()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartAgent {
    /// The fleet device to run on (this core's id included); absent = the
    /// machine the projects' folders are on.
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
    /// Start from the project's git remote, not the code machine's folder.
    #[serde(default)]
    from_remote: bool,
    /// Codex only: continue the task's session on that machine, with its own
    /// settings. Without one, a new session starts.
    #[serde(default)]
    resume: bool,
    /// Claude only: the name for a new git worktree (`--worktree`).
    #[serde(default)]
    worktree: Option<String>,
    /// Claude only: a worktree that already exists, to work in instead.
    #[serde(default)]
    worktree_path: Option<String>,
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
fn command(
    agent: &Agent,
    req: &StartAgent,
    slug: &str,
    task_id: &str,
    remote: bool,
    resume: Option<&SessionLink>,
) -> String {
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
            let existing = req.worktree_path.as_deref().filter(|p| !p.is_empty());
            if let (None, Some(name)) =
                (existing, req.worktree.as_deref().filter(|n| !n.is_empty()))
            {
                cmd += &format!(" --worktree {}", shell_quote(name));
            }
            let enter = existing.map_or(String::new(), |p| format!("cd {} && ", shell_quote(p)));
            format!("{enter}{cmd} -n \"{slug}\" {prompt}")
        }
        Agent::Codex => {
            let mut cmd = String::from("codex");
            if remote {
                cmd += CODEX_MACHINE_TOOLS;
            }
            cmd += CODEX_NOTIFY;
            if let Some(link) = resume {
                return format!(
                    "cd {} && {cmd} resume {}",
                    shell_quote(&link.cwd),
                    shell_quote(&link.session_id)
                );
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
    let code = state.code_machine();
    let target_id = req
        .device
        .clone()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| code.id().to_string());
    let Some(target) = super::fleet::target(&state, &target_id) else {
        return error(StatusCode::NOT_FOUND, "no such machine");
    };
    // Another core's sessions are reached over HTTP; this core's directly
    let device = (target.id != own_id).then(|| target.clone());
    // On the code machine the task works in the project's folder, unless it
    // has none (only a remote) or should start from the remote
    let in_place = target.id == code.id() && !directory.is_empty() && !req.from_remote;

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
    let path = if in_place {
        directory
    } else {
        match state
            .handoffs
            .start(
                workspace,
                &task.slug,
                &target,
                &code,
                req.include_uncommitted,
                req.from_remote,
            )
            .await
        {
            Ok((path, _branch)) => path,
            Err(err) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        }
    };
    let (session, path) = match &device {
        Some(device) => {
            match spawn_on_device(&state, device, &task.slug, &path, &agent, &req).await {
                Ok(session) => (session, path),
                Err(err) => return error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
            }
        }
        None => {
            let spawn = SpawnRequest {
                id: task.slug.clone(),
                argv: Vec::new(),
                cwd: Some(path.clone()),
                env: Default::default(),
                cols: req.cols,
                rows: req.rows,
                agent: Some(agent_name(&agent).into()),
                labels: Default::default(),
            };
            let registry = std::sync::Arc::clone(&state.registry);
            match tokio::task::spawn_blocking(move || registry.spawn(spawn)).await {
                Ok(Ok(info)) => (serde_json::to_value(info).unwrap_or(Value::Null), path),
                Ok(Err(err)) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
                Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
            }
        }
    };

    // The task remembers where it runs (none: in place, on the code machine)
    let device_id = (!in_place).then(|| target.id.clone());
    if let Some(change) = started_change(&task, device_id.as_deref()) {
        let ws = state.workspace.current();
        let slug = task.slug.clone();
        let updated =
            tokio::task::spawn_blocking(move || ws.map(|ws| ws.update_task(&slug, change))).await;
        if let Ok(Some(Err(err))) = updated {
            tracing::warn!(task = %task.slug, %err, "couldn't record that the task started");
        }
    }

    let resume = if req.resume && agent == Agent::Codex {
        codex_link(&state, device.as_ref(), &task.slug).await
    } else {
        None
    };
    let command = command(
        &agent,
        &req,
        &task.slug,
        &task.id,
        device.is_some(),
        resume.as_ref(),
    );
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
        "device": target.id,
        "path": path,
        "started": true,
    }))
    .into_response()
}

/// What starting an agent changes on its task: it's in progress from now
/// (also one that was completed), on that machine. None if it already says so.
fn started_change(task: &Task, device: Option<&str>) -> Option<UpdateTask> {
    let state = (task.state != "in-progress").then(|| "in-progress".to_string());
    let device = (task.device.as_deref() != device).then(|| device.map(str::to_string));
    (state.is_some() || device.is_some()).then(|| UpdateTask {
        state,
        device,
        ..Default::default()
    })
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
            from_remote: false,
            resume: false,
            worktree: None,
            worktree_path: None,
            cols: 80,
            rows: 24,
        }
    }

    fn task(state: &str, device: Option<&str>) -> Task {
        Task {
            state: state.into(),
            device: device.map(str::to_string),
            ..Task::default()
        }
    }

    #[test]
    fn starting_an_agent_should_put_its_task_in_progress() {
        let change = started_change(&task("not-started", None), None).expect("a change");
        assert_eq!(change.state.as_deref(), Some("in-progress"));
        assert_eq!(change.device, None);
        let reopened = started_change(&task("completed", None), None).expect("a change");
        assert_eq!(reopened.state.as_deref(), Some("in-progress"));
    }

    #[test]
    fn starting_an_agent_should_record_where_it_runs() {
        let moved = started_change(&task("in-progress", None), Some("cube")).expect("a change");
        assert_eq!(moved.state, None);
        assert_eq!(moved.device, Some(Some("cube".into())));
        let back = started_change(&task("in-progress", Some("cube")), None).expect("a change");
        assert_eq!(back.device, Some(None));
        assert!(started_change(&task("in-progress", Some("cube")), Some("cube")).is_none());
    }

    #[test]
    fn a_new_worktree_should_be_named_with_the_flag() {
        let mut req = request(None, None, None, false);
        req.worktree = Some("my-task".into());
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false, None),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" --worktree 'my-task' \
             -n \"t\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn an_existing_worktree_should_be_entered_instead_of_made() {
        let mut req = request(None, None, None, false);
        req.worktree = Some("my-task".into());
        req.worktree_path = Some("/code/it's/.claude/worktrees/my-task".into());
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false, None),
            "cd '/code/it'\\''s/.claude/worktrees/my-task' && \
             claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" \
             -n \"t\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn codex_should_ignore_worktrees() {
        let mut req = request(Some("gpt-6"), None, None, false);
        req.worktree = Some("my-task".into());
        req.worktree_path = Some("/somewhere".into());
        assert_eq!(
            command(&Agent::Codex, &req, "t", "26-001", false, None),
            format!("codex{CODEX_NOTIFY} --model 'gpt-6' \"Start working on task 26-001\"")
        );
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
            command(&Agent::Claude, &req, "my-task", "26-001", false, None),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" --permission-mode plan \
             --model 'claude-fable-5-1[1m]' --effort high --dangerously-skip-permissions \
             -n \"my-task\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn default_model_should_add_no_flag() {
        let req = request(Some("default"), None, None, false);
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false, None),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" -n \"t\" \"Start working on task 26-001\""
        );
    }

    #[test]
    fn codex_on_a_machine_should_get_the_tools_and_effort() {
        let req = request(Some("gpt-6"), Some("high"), None, false);
        let cmd = command(&Agent::Codex, &req, "t", "26-001", true, None);
        assert!(cmd.starts_with("codex -c 'mcp_servers.tui-do.command=\"sh\"'"));
        assert!(cmd.ends_with(
            " --model 'gpt-6' -c 'model_reasoning_effort=\"high\"' \"Start working on task 26-001\""
        ));
    }

    #[test]
    fn resuming_codex_should_continue_the_session_in_its_folder() {
        let req = request(Some("gpt-6"), Some("high"), None, true);
        let link = SessionLink {
            session_id: "01a1c173-979a-70e0-bbe0-b83e52032d3f".into(),
            cwd: "/code/it's".into(),
            linked_at: None,
        };
        assert_eq!(
            command(&Agent::Codex, &req, "t", "26-001", false, Some(&link)),
            format!(
                "cd '/code/it'\\''s' && codex{CODEX_NOTIFY} \
                 resume '01a1c173-979a-70e0-bbe0-b83e52032d3f'"
            )
        );
    }

    #[test]
    fn odd_effort_and_mode_values_should_be_dropped() {
        let req = request(None, Some("high; rm -rf ~"), Some("$(x)"), false);
        assert_eq!(
            command(&Agent::Claude, &req, "t", "26-001", false, None),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" -n \"t\" \"Start working on task 26-001\""
        );
    }
}
