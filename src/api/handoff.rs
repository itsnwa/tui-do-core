//! Handoff routes: on the home core, starting a task on a device and its pull
//! request; on a device, receiving or fetching a project and handing the work
//! back; on the code machine, snapshotting a project and landing returned
//! work (see `handoff`).

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
use crate::handoff::{
    CodeMachine, FetchRequest, Handoffs, SNAPSHOT_HEADER, SnapshotParams, SnapshotRequest, github,
};
use crate::workspace::projects::UpdateProject;

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
        .route("/v0/handoff/{project}/{task}/fetch", post(fetch))
        .route("/v0/handoff/{project}/{task}/publish", post(publish))
        .route("/v0/tasks/{slug}/pr", get(pull_request))
        .route("/v0/tasks/{slug}/pr/merge", post(merge_pull_request))
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
    /// Start from the project's git remote, not the code machine's folder.
    #[serde(default)]
    from_remote: bool,
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
        .start(
            workspace,
            &slug,
            &device,
            &code,
            body.include_uncommitted,
            body.from_remote,
        )
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

/// Device: fetch a project from its remote for a task.
async fn fetch(
    State(state): State<AppState>,
    Path((project, task)): Path<(String, String)>,
    Json(req): Json<FetchRequest>,
) -> Response {
    let handoffs = Arc::clone(&state.handoffs);
    match tokio::task::spawn_blocking(move || handoffs.fetch(&project, &task, &req)).await {
        Ok(Ok(fetched)) => Json(fetched).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct PublishBody {
    remote: String,
}

/// Device: push a task's work to the remote it was fetched from.
async fn publish(
    State(state): State<AppState>,
    Path((project, task)): Path<(String, String)>,
    Query(params): Query<SnapshotParams>,
    Json(body): Json<PublishBody>,
) -> Response {
    let handoffs = Arc::clone(&state.handoffs);
    match tokio::task::spawn_blocking(move || {
        handoffs.publish(&project, &task, &params, &body.remote)
    })
    .await
    {
        Ok(Ok(pushed)) => Json(json!({ "pushed": pushed })).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Home: the task's pull request, as GitHub has it now.
async fn pull_request(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let found = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let pr = workspace.get_task(&slug)?.task.pr;
        let pr = pr.ok_or_else(|| anyhow::anyhow!("the task has no pull request"))?;
        github::view_pr(&pr)
    })
    .await;
    match found {
        Ok(Ok(pr)) => Json(pr).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Home: squash-merge the task's pull request; answers with it merged.
async fn merge_pull_request(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let merged = tokio::task::spawn_blocking(move || -> anyhow::Result<_> {
        let pr = workspace.get_task(&slug)?.task.pr;
        let pr = pr.ok_or_else(|| anyhow::anyhow!("the task has no pull request"))?;
        github::merge_pr(&pr)?;
        github::view_pr(&pr)
    })
    .await;
    match merged {
        Ok(Ok(pr)) => Json(pr).into_response(),
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

/// Whether a folder on this machine is a git repo, how many files have
/// uncommitted changes, and its `origin`.
async fn git_status(Query(query): Query<GitStatusQuery>) -> Response {
    let dir = crate::session::expand_home(&query.dir);
    let status = tokio::task::spawn_blocking(move || {
        let (is_repo, uncommitted) = crate::handoff::git::status(&dir);
        let remote = crate::handoff::git::origin(&dir).unwrap_or_default();
        (is_repo, uncommitted, remote)
    })
    .await;
    match status {
        Ok((is_repo, uncommitted, remote)) => Json(json!({
            "isRepo": is_repo,
            "uncommitted": uncommitted,
            "remote": remote,
        }))
        .into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Home: whether a project is a git repo, how many files have uncommitted
/// changes and its remote, asked of the code machine. A project learns its
/// remote from the folder's `origin`. With the code machine away (or no
/// folder), `fromRemote` says other machines would fetch the remote instead.
async fn project_git(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    let Some(workspace) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let ws = Arc::clone(&workspace);
    let project = match tokio::task::spawn_blocking(move || ws.get_project(&slug)).await {
        Ok(Ok(project)) => project,
        Ok(Err(err)) => return error(StatusCode::NOT_FOUND, &err.to_string()),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    // What's known without the code machine: other machines fetch the remote
    let known = json!({
        "isRepo": !project.remote.is_empty(),
        "uncommitted": 0,
        "remote": project.remote,
        "fromRemote": !project.remote.is_empty(),
    });
    if project.directory.is_empty() {
        return Json(known).into_response();
    }
    let status = match state.code_machine() {
        CodeMachine::Here(_) => {
            let dir = crate::session::expand_home(&project.directory);
            tokio::task::spawn_blocking(move || {
                let (is_repo, uncommitted) = crate::handoff::git::status(&dir);
                let remote = crate::handoff::git::origin(&dir).unwrap_or_default();
                json!({ "isRepo": is_repo, "uncommitted": uncommitted, "remote": remote })
            })
            .await
            .map_err(|err| err.to_string())
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
                    Ok(res.json::<serde_json::Value>().await.unwrap_or_default())
                }
                Ok(res) => Err(format!("{} answered {}", device.name, res.status())),
                Err(err) => Err(format!("{} isn't reachable: {err}", device.name)),
            }
        }
    };
    let mut status = match status {
        Ok(status) => status,
        // The project can still start from its remote
        Err(_) if !project.remote.is_empty() => return Json(known).into_response(),
        Err(err) => return error(StatusCode::BAD_GATEWAY, &err),
    };
    let origin = status["remote"].as_str().unwrap_or_default().to_string();
    if origin.is_empty() {
        status["remote"] = project.remote.into();
    } else if project.remote.is_empty() {
        let slug = project.slug;
        let _ = tokio::task::spawn_blocking(move || {
            workspace.update_project(
                &slug,
                UpdateProject {
                    remote: Some(origin),
                    ..UpdateProject::default()
                },
            )
        })
        .await;
    }
    Json(status).into_response()
}

#[derive(Deserialize)]
struct SetupBody {
    instructions: String,
}

/// Register the TUI-DO tools with the coding agents installed on this machine.
async fn setup_agents(State(state): State<AppState>, Json(body): Json<SetupBody>) -> Response {
    let core_dir = Arc::clone(&state.core_dir);
    match tokio::task::spawn_blocking(move || crate::agents::setup(&body.instructions, &core_dir))
        .await
    {
        Ok(Ok(done)) => Json(done).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}
