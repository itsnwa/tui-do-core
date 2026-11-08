//! Handoff routes: on the home core, starting a task on a device; on a
//! device, receiving a project and handing the work back; on the code
//! machine, snapshotting a project and landing returned work (see `handoff`).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};
use crate::handoff::{CodeMachine, Handoffs, SNAPSHOT_HEADER, SnapshotParams, SnapshotRequest};

/// Bundles can be large (a whole repo's history the first time).
const MAX_BUNDLE: usize = 2 * 1024 * 1024 * 1024;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/tasks/{slug}/handoff", post(start))
        .route("/v0/handoff/{project}/commits", get(commits))
        .route("/v0/projects/{slug}/git", get(project_git))
        .route("/v0/agents/setup", put(setup_agents))
        .route(
            "/v0/handoff/{project}/{task}",
            post(receive).layer(DefaultBodyLimit::max(MAX_BUNDLE)),
        )
        .route("/v0/handoff/{project}/{task}/collect", post(collect))
        .route("/v0/handoff/{project}/{task}/snapshot", post(snapshot))
        .route(
            "/v0/handoff/{project}/{task}/land",
            post(land).layer(DefaultBodyLimit::max(MAX_BUNDLE)),
        )
        .route("/v0/git/status", get(git_status))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StartBody {
    device: String,
    /// Take the project's uncommitted changes along (off: commits only).
    #[serde(default)]
    include_uncommitted: bool,
}

/// Home: hand the task's project to a device; answers with its folder there.
async fn start(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<StartBody>,
) -> Response {
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let Some(device) = super::fleet::target(&state, &body.device) else {
        return error(StatusCode::NOT_FOUND, "no such machine");
    };
    let code = state.code_machine();
    match state
        .handoffs
        .start(workspace, &slug, &device, &code, body.include_uncommitted)
        .await
    {
        Ok((path, branch)) => {
            Json(json!({ "device": device.id, "path": path, "branch": branch })).into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}

async fn commits(State(state): State<AppState>, Path(project): Path<String>) -> Response {
    let handoffs = Arc::clone(&state.handoffs);
    match tokio::task::spawn_blocking(move || handoffs.known_commits(&project)).await {
        Ok(Ok(commits)) => Json(commits).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn receive(
    State(state): State<AppState>,
    Path((project, task)): Path<(String, String)>,
    Query(params): Query<SnapshotParams>,
    bundle: Bytes,
) -> Response {
    let handoffs = Arc::clone(&state.handoffs);
    match tokio::task::spawn_blocking(move || handoffs.receive(&project, &task, &params, &bundle))
        .await
    {
        Ok(Ok(path)) => Json(json!({ "path": path })).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn collect(
    State(state): State<AppState>,
    Path((project, task)): Path<(String, String)>,
    Query(params): Query<SnapshotParams>,
) -> Response {
    let handoffs = Arc::clone(&state.handoffs);
    match tokio::task::spawn_blocking(move || handoffs.collect(&project, &task, &params)).await {
        Ok(Ok(Some(bundle))) => bundle.into_response(),
        Ok(Ok(None)) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Code machine: snapshot a project for the home core. The bundle is the
/// body; the snapshot is in the `x-tui-do-snapshot` header.
async fn snapshot(
    Path((project, task)): Path<(String, String)>,
    Json(req): Json<SnapshotRequest>,
) -> Response {
    let made =
        tokio::task::spawn_blocking(move || Handoffs::snapshot_here(&project, &task, &req)).await;
    match made {
        Ok(Ok((snapshot, bundle))) => {
            let header = serde_json::to_string(&snapshot)
                .ok()
                .and_then(|h| HeaderValue::from_str(&h).ok());
            let Some(header) = header else {
                return error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "the repository's path can't be sent",
                );
            };
            let mut response = bundle.unwrap_or_default().into_response();
            response.headers_mut().insert(SNAPSHOT_HEADER, header);
            response
        }
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct LandQuery {
    repo: std::path::PathBuf,
    branch: String,
}

/// Code machine: fetch a task's returned work into the project.
async fn land(
    Path((_project, task)): Path<(String, String)>,
    Query(query): Query<LandQuery>,
    bundle: Bytes,
) -> Response {
    let landed = tokio::task::spawn_blocking(move || {
        Handoffs::land_here(&task, &query.repo, &query.branch, &bundle)
    })
    .await;
    match landed {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct GitStatusQuery {
    dir: String,
}

/// Whether a folder on this machine is a git repo, and how many files have
/// uncommitted changes.
async fn git_status(Query(query): Query<GitStatusQuery>) -> Response {
    let dir = crate::session::expand_home(&query.dir);
    match tokio::task::spawn_blocking(move || crate::handoff::git::status(&dir)).await {
        Ok((is_repo, uncommitted)) => {
            Json(json!({ "isRepo": is_repo, "uncommitted": uncommitted })).into_response()
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Home: whether a project is a git repo and how many files have uncommitted
/// changes, asked of the code machine.
async fn project_git(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let project = match tokio::task::spawn_blocking(move || workspace.get_project(&slug)).await {
        Ok(Ok(project)) => project,
        Ok(Err(err)) => return error(StatusCode::NOT_FOUND, &err.to_string()),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    if project.directory.is_empty() {
        return Json(json!({ "isRepo": false, "uncommitted": 0 })).into_response();
    }
    match state.code_machine() {
        CodeMachine::Here(_) => {
            git_status(Query(GitStatusQuery {
                dir: project.directory,
            }))
            .await
        }
        CodeMachine::Device(device) => {
            let sent = state
                .fleet
                .client()
                .get(format!("{}/v0/git/status", device.address))
                .bearer_auth(&device.token)
                .query(&[("dir", &project.directory)])
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await;
            match sent {
                Ok(res) if res.status().is_success() => {
                    let value: serde_json::Value = res.json().await.unwrap_or_default();
                    Json(value).into_response()
                }
                Ok(res) => error(
                    StatusCode::BAD_GATEWAY,
                    &format!("{} answered {}", device.name, res.status()),
                ),
                Err(err) => error(
                    StatusCode::BAD_GATEWAY,
                    &format!("{} isn't reachable: {err}", device.name),
                ),
            }
        }
    }
}

#[derive(Deserialize)]
struct SetupBody {
    instructions: String,
}

/// Register the TUI-DO tools with the coding agents installed on this machine.
async fn setup_agents(Json(body): Json<SetupBody>) -> Response {
    match tokio::task::spawn_blocking(move || crate::agents::setup(&body.instructions)).await {
        Ok(Ok(done)) => Json(done).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}
