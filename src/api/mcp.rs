//! MCP routes: the streamable HTTP endpoint `/mcp`, the call endpoint that
//! `tui-do-core mcp` forwards to (the Assistant's tools included), and the
//! task ↔ Claude session links.

use std::sync::Arc;

use axum::Json;
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::{AppState, error};
use crate::clients::Caller;
use crate::links::SessionLink;
use crate::mcp::{CallOutcome, ForwardedCall, TuiDoMcp, run_call, tools};

pub fn routes(state: &AppState) -> axum::Router<AppState> {
    let (workspace, sessions, config, handoffs) = (
        Arc::clone(&state.workspace),
        Arc::clone(&state.sessions),
        Arc::clone(&state.config),
        Arc::clone(&state.handoffs),
    );
    let service = StreamableHttpService::new(
        move || {
            Ok(TuiDoMcp::local(
                Arc::clone(&workspace),
                Arc::clone(&sessions),
                Arc::clone(&config),
                Arc::clone(&handoffs),
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
    caller: Option<Extension<Caller>>,
    Json(call): Json<ForwardedCall>,
) -> Response {
    if call.assistant && tools::is_assistant_tool(&call.name) {
        // Only from this machine (the core's own token, not a device's
        // MCP-only one): the Assistant runs on the home
        let outcome = if matches!(caller.as_deref(), Some(Caller::Owner)) {
            super::assistant::call_tool(&state, &call.name, call.arguments).await
        } else {
            CallOutcome::Error("Only the TUI-DO Assistant can use this tool.".into())
        };
        return Json(outcome).into_response();
    }
    let outcome = run_call(
        Arc::clone(&state.workspace),
        Arc::clone(&state.sessions),
        state.config.get().home,
        Some(Arc::clone(&state.handoffs)),
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
