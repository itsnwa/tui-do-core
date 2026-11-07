//! MCP routes: the streamable HTTP endpoint `/mcp`, the call endpoint that
//! `tui-do-core mcp` forwards to, and the task ↔ Claude session links.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::{AppState, error};
use crate::links::SessionLink;
use crate::mcp::{ForwardedCall, TuiDoMcp, run_call};

pub fn routes(state: &AppState) -> axum::Router<AppState> {
    let (workspace, sessions) = (Arc::clone(&state.workspace), Arc::clone(&state.sessions));
    let service = StreamableHttpService::new(
        move || {
            Ok(TuiDoMcp::local(
                Arc::clone(&workspace),
                Arc::clone(&sessions),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new()
        .nest_service("/mcp", service)
        .route("/v0/mcp/call", post(forwarded_call))
        .route(
            "/v0/tasks/{slug}/claude-session",
            get(get_link).put(set_link),
        )
}

async fn forwarded_call(
    State(state): State<AppState>,
    Json(call): Json<ForwardedCall>,
) -> Response {
    let outcome = run_call(
        Arc::clone(&state.workspace),
        Arc::clone(&state.sessions),
        call,
    )
    .await;
    Json(outcome).into_response()
}

async fn get_link(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    match state.sessions.get(&slug) {
        Some(link) => Json(link).into_response(),
        None => error(
            StatusCode::NOT_FOUND,
            "no Claude session linked to this task",
        ),
    }
}

async fn set_link(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(link): Json<SessionLink>,
) -> Response {
    match state.sessions.set(&slug, &link) {
        Ok(stored) => Json(stored).into_response(),
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}
