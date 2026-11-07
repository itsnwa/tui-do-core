//! HTTP + WebSocket API, version 0.
//!
//! Every route except `/v0/health` needs the core's token, as
//! `Authorization: Bearer <token>` or a `?token=` query parameter (browsers
//! can't set headers on WebSockets).
//!
//! WebSocket messages are JSON text. A `snapshot` message always means "reset
//! the terminal, then write this": it's sent on attach and again whenever a
//! client fell too far behind to receive every output event.

use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::session::{Event, Registry, SpawnRequest};

pub const API_VERSION: u32 = 0;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub token: Arc<str>,
}

pub fn router(state: AppState) -> Router {
    let authed = Router::new()
        .route("/v0/sessions", get(list_sessions).post(spawn_session))
        .route("/v0/sessions/{id}", get(get_session).delete(remove_session))
        .route("/v0/sessions/{id}/input", post(input))
        .route("/v0/sessions/{id}/resize", post(resize))
        .route("/v0/sessions/{id}/snapshot", get(snapshot))
        .route("/v0/sessions/{id}/attach", get(attach))
        .route("/v0/events", get(events))
        .route_layer(middleware::from_fn_with_state(state.clone(), require_token));
    Router::new()
        .route("/v0/health", get(health))
        .merge(authed)
        .with_state(state)
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn require_token(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let from_header = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let from_query = req
        .uri()
        .query()
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("token=")));
    let ok = from_header
        .or(from_query)
        .is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()));
    if ok {
        next.run(req).await
    } else {
        error(StatusCode::UNAUTHORIZED, "missing or wrong token")
    }
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such session")
}

async fn health() -> Json<serde_json::Value> {
    Json(json!({
        "name": "tui-do-core",
        "version": env!("CARGO_PKG_VERSION"),
        "apiVersion": API_VERSION,
    }))
}

async fn list_sessions(State(state): State<AppState>) -> Response {
    Json(state.registry.list()).into_response()
}

async fn get_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    state
        .registry
        .info(&id)
        .map_or_else(not_found, |info| Json(info).into_response())
}

async fn spawn_session(State(state): State<AppState>, Json(req): Json<SpawnRequest>) -> Response {
    let registry = Arc::clone(&state.registry);
    // Spawning forks and waits on the session thread; keep it off the async workers.
    match tokio::task::spawn_blocking(move || registry.spawn(req)).await {
        Ok(Ok(info)) => Json(info).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn remove_session(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.registry.remove(&id) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

#[derive(Deserialize)]
struct InputBody {
    data: String,
}

async fn input(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<InputBody>,
) -> Response {
    if state.registry.input(&id, body.data.into_bytes()) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

#[derive(Deserialize)]
struct ResizeBody {
    cols: u16,
    rows: u16,
}

async fn resize(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<ResizeBody>,
) -> Response {
    if state.registry.resize(&id, body.cols, body.rows) {
        StatusCode::NO_CONTENT.into_response()
    } else {
        not_found()
    }
}

async fn snapshot(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.registry.snapshot(&id).await {
        Some(snap) => Json(json!({
            "data": snap.data,
            "alive": snap.info.alive,
            "exitCode": snap.info.exit_code,
            "session": snap.info,
        }))
        .into_response(),
        None => not_found(),
    }
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum ClientMessage {
    Input { data: String },
    Resize { cols: u16, rows: u16 },
}

async fn send_json(socket: &mut WebSocket, value: &impl serde::Serialize) -> bool {
    match serde_json::to_string(value) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

async fn attach(
    State(state): State<AppState>,
    Path(id): Path<String>,
    ws: WebSocketUpgrade,
) -> Response {
    if state.registry.info(&id).is_none() {
        return not_found();
    }
    ws.on_upgrade(move |socket| attach_loop(socket, state.registry, id))
}

async fn send_snapshot(
    socket: &mut WebSocket,
    registry: &Registry,
    id: &str,
) -> Option<tokio::sync::broadcast::Receiver<Event>> {
    let (data, rx) = registry.attach(id).await?;
    let session = registry.info(id)?;
    send_json(
        socket,
        &json!({ "type": "snapshot", "data": data, "session": session }),
    )
    .await
    .then_some(rx)
}

async fn attach_loop(mut socket: WebSocket, registry: Arc<Registry>, id: String) {
    let Some(mut rx) = send_snapshot(&mut socket, &registry, &id).await else {
        return;
    };
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(event) if event.session_id() == id => {
                    let removed = matches!(event, Event::SessionRemoved { .. });
                    if !send_json(&mut socket, &event).await || removed {
                        break;
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    match send_snapshot(&mut socket, &registry, &id).await {
                        Some(fresh) => rx = fresh,
                        None => break,
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Input { data }) => { registry.input(&id, data.into_bytes()); }
                    Ok(ClientMessage::Resize { cols, rows }) => { registry.resize(&id, cols, rows); }
                    Err(err) => tracing::debug!(%err, "ignoring client message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}

/// Messages a client may send on `/v0/events`, so one connection can drive
/// every session in order.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum EventsClientMessage {
    Input { id: String, data: String },
    Resize { id: String, cols: u16, rows: u16 },
}

#[derive(Deserialize)]
struct EventsQuery {
    /// Include every session's output (for clients mirroring all sessions).
    #[serde(default)]
    output: Option<String>,
}

async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let with_output = matches!(query.output.as_deref(), Some("1" | "true"));
    ws.on_upgrade(move |socket| events_loop(socket, state.registry, with_output))
}

async fn events_loop(mut socket: WebSocket, registry: Arc<Registry>, with_output: bool) {
    let mut rx = registry.subscribe();
    // Clients sync from this list, then apply events in order.
    if !send_json(
        &mut socket,
        &json!({ "type": "sessions", "sessions": registry.list() }),
    )
    .await
    {
        return;
    }
    loop {
        tokio::select! {
            event = rx.recv() => match event {
                Ok(Event::Output { .. }) if !with_output => {}
                Ok(event) => {
                    if !send_json(&mut socket, &event).await {
                        break;
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // Missed events: resync. Clients mirroring output should re-fetch snapshots.
                    rx = registry.subscribe();
                    if !send_json(&mut socket, &json!({ "type": "resync", "sessions": registry.list() })).await {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<EventsClientMessage>(&text) {
                    Ok(EventsClientMessage::Input { id, data }) => { registry.input(&id, data.into_bytes()); }
                    Ok(EventsClientMessage::Resize { id, cols, rows }) => { registry.resize(&id, cols, rows); }
                    Err(err) => tracing::debug!(%err, "ignoring client message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
}
