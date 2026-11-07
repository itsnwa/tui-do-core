//! MCP routes: the streamable HTTP endpoint `/mcp`, the call endpoint that
//! `tui-do-core mcp` forwards to, and the task ↔ Claude session links.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::{AppState, error};
use crate::links::SessionLink;
use crate::mcp::{ForwardedCall, TuiDoMcp, run_call};

pub fn routes(state: &AppState) -> axum::Router<AppState> {
    let (workspace, sessions, config) = (
        Arc::clone(&state.workspace),
        Arc::clone(&state.sessions),
        Arc::clone(&state.config),
    );
    let service = StreamableHttpService::new(
        move || {
            Ok(TuiDoMcp::local(
                Arc::clone(&workspace),
                Arc::clone(&sessions),
                Arc::clone(&config),
            ))
        },
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new()
        .nest_service("/mcp", service)
        .route("/v0/mcp/call", post(forwarded_call))
        .route("/v0/mcp/tokens", post(issue_token))
        .route("/v0/mcp/tokens/{token}", delete(revoke_token))
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
        state.config.get().home,
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

/// Tokens that grant access to `/mcp` only. A remote ssh session gets one
/// (through its reverse tunnel), so the remote machine can use the TUI-DO
/// tools without being able to start terminal sessions on this one. They live
/// in memory: a core restart ends sessions and their tokens together.
#[derive(Default)]
pub struct McpTokens(Mutex<HashSet<String>>);

impl McpTokens {
    pub fn contains(&self, token: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .contains(token)
    }
}

async fn issue_token(State(state): State<AppState>) -> Response {
    let token = uuid::Uuid::new_v4().simple().to_string();
    state
        .mcp_tokens
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(token.clone());
    Json(serde_json::json!({ "token": token })).into_response()
}

async fn revoke_token(State(state): State<AppState>, Path(token): Path<String>) -> Response {
    state
        .mcp_tokens
        .0
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .remove(&token);
    StatusCode::NO_CONTENT.into_response()
}
