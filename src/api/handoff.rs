//! Handoff routes: on the home core, starting a task on a device; on a
//! device, receiving a project and handing the work back (see `handoff`).

use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};
use crate::handoff::SnapshotParams;

/// Bundles can be large (a whole repo's history the first time).
const MAX_BUNDLE: usize = 2 * 1024 * 1024 * 1024;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/tasks/{slug}/handoff", post(start))
        .route("/v0/handoff/{project}/commits", get(commits))
        .route(
            "/v0/handoff/{project}/{task}",
            post(receive).layer(DefaultBodyLimit::max(MAX_BUNDLE)),
        )
        .route("/v0/handoff/{project}/{task}/collect", post(collect))
}

#[derive(Deserialize)]
struct StartBody {
    device: String,
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
    let Some(device) = state.fleet.device(&body.device) else {
        return error(StatusCode::NOT_FOUND, "no such machine");
    };
    match state.handoffs.start(workspace, &slug, &device).await {
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
