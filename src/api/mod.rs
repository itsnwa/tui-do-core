//! HTTP + WebSocket API, version 0.
//!
//! Every route except `/v0/health` needs the core's token or a paired
//! client's token (see `clients`), as `Authorization: Bearer <token>` or a
//! `?token=` query parameter (browsers can't set headers on WebSockets).
//!
//! WebSocket messages are JSON text. A `snapshot` message always means "reset
//! the terminal, then write this": it's sent on attach and again whenever a
//! client fell too far behind to receive every output event. A client that
//! attaches with `?paced=1` acknowledges what it gets (see `pace`).

mod agent;
mod clients;
mod fleet;
mod handoff;
mod home;
pub(crate) mod mcp;
mod update;
mod workspace;

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use crate::clients::{Caller, Clients, Revocation};
use crate::config::ConfigStore;
use crate::links::TaskSessions;
use crate::pace::{self, Pacer};
use crate::session::{ClientId, Event, Registry, SpawnRequest};
use crate::workspace::shared::SharedWorkspace;

pub use home::spawn_background;

pub const API_VERSION: u32 = 0;

#[derive(Clone)]
pub struct AppState {
    pub registry: Arc<Registry>,
    pub config: Arc<ConfigStore>,
    pub fleet: Arc<crate::fleet::Fleet>,
    pub relays: Arc<crate::relay::Relays>,
    pub handoffs: Arc<crate::handoff::Handoffs>,
    /// This core's loopback base URL, for requests addressed to its own device id.
    pub local_endpoint: Arc<str>,
    pub workspace: Arc<SharedWorkspace>,
    pub sessions: Arc<TaskSessions>,
    pub token: Arc<str>,
    pub clients: Arc<Clients>,
    pub replicas: Arc<crate::replica::Replicas>,
    /// This core's folder (`~/.tui-do/core`).
    pub core_dir: Arc<std::path::Path>,
    /// Held while the home moves away from this core.
    pub moving: Arc<tokio::sync::Mutex<()>>,
    /// Held while this core takes a home link.
    pub linking: Arc<tokio::sync::Mutex<()>>,
    /// The app tokens this home has made, by device id.
    pub app_tokens: Arc<std::sync::Mutex<std::collections::HashMap<String, String>>>,
}

impl AppState {
    /// The machine the projects' folders are on.
    pub(crate) fn code_machine(&self) -> crate::handoff::CodeMachine {
        use crate::handoff::CodeMachine;
        let own = self.config.get().device_id;
        let id = self.fleet.code_device(&own);
        match self.fleet.device(&id) {
            Some(device) if id != own => CodeMachine::Device(device),
            _ => CodeMachine::Here(own),
        }
    }
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
        .route("/v0/device", get(get_device).patch(update_device))
        .merge(workspace::routes())
        .merge(agent::routes())
        .merge(clients::routes())
        .merge(fleet::routes())
        .merge(home::routes())
        .merge(handoff::routes())
        .merge(update::routes())
        .merge(mcp::routes(&state))
        .route_layer(middleware::from_fn_with_state(state.clone(), guard_writes))
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
    let presented = from_header.or(from_query);
    let full = presented.is_some_and(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()));
    // Each fleet device's MCP-only token opens the MCP endpoints and nothing else
    let path = req.uri().path();
    let mcp_only = (path.starts_with("/mcp") || path == "/v0/mcp/call")
        && presented.is_some_and(|t| state.fleet.is_mcp_token(t));
    let caller = if full {
        Caller::Owner
    } else if let Some(client) = presented.and_then(|t| state.clients.authenticate(t)) {
        // A paired client can't make or revoke client tokens, here or through
        // the device proxy (which forwards with the core's own token), except
        // the app on a Mac the home moved away from, here
        let manages = clients::manages_clients(path) && state.clients.is_manager(&client);
        if clients::owner_only(path) && !manages {
            return error(
                StatusCode::FORBIDDEN,
                "only the core's own token can use this",
            );
        }
        Caller::Client(client)
    } else if mcp_only {
        return next.run(req).await;
    } else {
        return error(StatusCode::UNAUTHORIZED, "missing or wrong token");
    };
    let mut req = req;
    req.extensions_mut().insert(caller);
    next.run(req).await
}

/// Routes that change the workspace (or the home's records of tasks).
fn writes_workspace(req: &Request) -> bool {
    if matches!(
        *req.method(),
        axum::http::Method::GET | axum::http::Method::HEAD
    ) {
        return false;
    }
    let path = req.uri().path();
    [
        "/v0/tasks",
        "/v0/projects",
        "/v0/plans",
        "/v0/daily",
        "/v0/weekly",
        "/v0/workspace",
    ]
    .iter()
    .any(|prefix| {
        path.strip_prefix(prefix)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
    })
}

/// A runner's workspace is a read-only copy, and the home takes no writes
/// while it moves.
async fn guard_writes(State(state): State<AppState>, req: Request, next: Next) -> Response {
    if writes_workspace(&req) {
        // Held until the write is done, so a move waits for it
        let _writing = state.workspace.writing().await;
        let config = state.config.get();
        if let Some(home) = &config.home {
            return (
                StatusCode::CONFLICT,
                Json(json!({
                    "error": format!("This machine has a read-only copy of the tasks; changes go to the home, {}.", home.name),
                    "home": home::home_summary(&state),
                })),
            )
                .into_response();
        }
        if state.workspace.is_frozen() {
            return error(
                StatusCode::SERVICE_UNAVAILABLE,
                "The home is moving to another machine; try again in a moment.",
            );
        }
        return next.run(req).await;
    }
    next.run(req).await
}

fn error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "no such session")
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    let config = state.config.get();
    Json(json!({
        "name": "tui-do-core",
        "version": env!("CARGO_PKG_VERSION"),
        "apiVersion": API_VERSION,
        "device": { "id": config.device_id, "name": config.device_name },
        "home": home::home_summary(&state),
    }))
}

/// This core's identity and network settings.
async fn device_info(state: &AppState) -> serde_json::Value {
    let config = state.config.get();
    let tailscale = crate::net::tailscale_ipv4().await.map(|ip| ip.to_string());
    json!({
        "id": config.device_id,
        "name": config.device_name,
        "remoteAccess": config.remote_access,
        "tailscaleAddress": tailscale,
        "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

async fn get_device(State(state): State<AppState>) -> Response {
    Json(device_info(&state).await).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DeviceChange {
    name: Option<String>,
    remote_access: Option<bool>,
}

async fn update_device(
    State(state): State<AppState>,
    Json(change): Json<DeviceChange>,
) -> Response {
    if change.name.as_deref().is_some_and(|n| n.trim().is_empty()) {
        return error(StatusCode::BAD_REQUEST, "the device name can't be empty");
    }
    let updated = state.config.update(|c| {
        if let Some(name) = change.name {
            c.device_name = name.trim().to_string();
        }
        if let Some(remote) = change.remote_access {
            c.remote_access = remote;
        }
    });
    match updated {
        Ok(_) => Json(device_info(&state).await).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
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
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Send the screen again (e.g. after another client changed the size,
    /// so the client's emulator matches the core's exactly).
    Snapshot,
    /// A paced client has the oldest `output` or `snapshot` not yet acknowledged.
    Ack,
}

async fn send_json(socket: &mut WebSocket, value: &impl serde::Serialize) -> bool {
    match serde_json::to_string(value) {
        Ok(text) => socket.send(Message::Text(text.into())).await.is_ok(),
        Err(_) => false,
    }
}

/// An attaching client's size, taken before the snapshot so it arrives at that size.
#[derive(Deserialize)]
struct AttachQuery {
    cols: Option<u16>,
    rows: Option<u16>,
    /// The client acknowledges what it gets, so its output can be paced.
    paced: Option<String>,
}

async fn attach(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<AttachQuery>,
    caller: Option<Extension<Caller>>,
    ws: WebSocketUpgrade,
) -> Response {
    if state.registry.info(&id).is_none() {
        return not_found();
    }
    let revocation = Revocation::new(&state.clients, caller.as_deref());
    let size = query.cols.zip(query.rows);
    let paced = matches!(query.paced.as_deref(), Some("1" | "true"));
    ws.on_upgrade(move |socket| attach_loop(socket, state.registry, id, size, paced, revocation))
}

async fn send_snapshot(
    socket: &mut WebSocket,
    registry: &Registry,
    id: &str,
    pacer: Option<&mut Pacer>,
) -> Option<tokio::sync::broadcast::Receiver<Event>> {
    let (data, rx) = registry.attach(id).await?;
    let session = registry.info(id)?;
    if let Some(pacer) = pacer {
        // The snapshot has everything that was waiting
        pacer.clear();
        pacer.sent(data.len());
    }
    send_json(
        socket,
        &json!({ "type": "snapshot", "data": data, "session": session }),
    )
    .await
    .then_some(rx)
}

/// Carry out what the pacer decided. False once the connection is done.
async fn send_paced(
    socket: &mut WebSocket,
    registry: &Registry,
    id: &str,
    pacer: &mut Pacer,
    rx: &mut tokio::sync::broadcast::Receiver<Event>,
    next: pace::Next,
) -> bool {
    let data = match next {
        pace::Next::Wait => return true,
        pace::Next::Send(data) => data,
        pace::Next::Repaint => {
            let Some((screen, fresh)) = registry.repaint(id).await else {
                return false;
            };
            let mut stale = std::mem::replace(rx, fresh);
            // That output is on the screen; anything else still has to arrive
            while let Ok(event) = stale.try_recv() {
                if event.session_id() == id
                    && !matches!(event, Event::Output { .. })
                    && !send_json(socket, &event).await
                {
                    return false;
                }
            }
            pacer.sent(screen.len());
            screen
        }
    };
    send_json(socket, &json!({ "type": "output", "id": id, "data": data })).await
}

async fn attach_loop(
    mut socket: WebSocket,
    registry: Arc<Registry>,
    id: String,
    size: Option<(u16, u16)>,
    paced: bool,
    mut revocation: Revocation,
) {
    let client = registry.connect();
    if let Some((cols, rows)) = size {
        registry.resize_from(&id, client, cols, rows);
    }
    let mut pacer = paced.then(Pacer::default);
    let Some(mut rx) = send_snapshot(&mut socket, &registry, &id, pacer.as_mut()).await else {
        registry.detach(&id, client);
        return;
    };
    loop {
        tokio::select! {
            () = revocation.revoked() => break,
            event = rx.recv() => match event {
                Ok(event) if event.session_id() == id => {
                    if let (Some(pacer), Event::Output { data, .. }) = (&mut pacer, &event) {
                        pacer.push(data, Instant::now());
                    } else {
                        // These only make sense after the output before them
                        let ordered = matches!(
                            event,
                            Event::SessionResized { .. } | Event::SessionExited { .. } | Event::SessionRemoved { .. }
                        );
                        if let Some(pacer) = pacer.as_mut().filter(|_| ordered) {
                            let next = pacer.flush();
                            if !send_paced(&mut socket, &registry, &id, pacer, &mut rx, next).await {
                                break;
                            }
                        }
                        let removed = matches!(event, Event::SessionRemoved { .. });
                        if !send_json(&mut socket, &event).await || removed {
                            break;
                        }
                    }
                }
                Ok(_) => {}
                Err(RecvError::Lagged(_)) => {
                    match send_snapshot(&mut socket, &registry, &id, pacer.as_mut()).await {
                        Some(fresh) => rx = fresh,
                        None => break,
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMessage>(&text) {
                    Ok(ClientMessage::Input { data }) => { registry.input_from(&id, client, data.into_bytes()); }
                    Ok(ClientMessage::Resize { cols, rows }) => { registry.resize_from(&id, client, cols, rows); }
                    Ok(ClientMessage::Snapshot) => match send_snapshot(&mut socket, &registry, &id, pacer.as_mut()).await {
                        Some(fresh) => rx = fresh,
                        None => break,
                    },
                    Ok(ClientMessage::Ack) => {
                        if let Some(pacer) = &mut pacer {
                            pacer.ack();
                        }
                    }
                    Err(err) => tracing::debug!(%err, "ignoring client message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
        if let Some(pacer) = &mut pacer {
            let mut open = true;
            while open {
                match pacer.next(Instant::now()) {
                    pace::Next::Wait => break,
                    next => {
                        open = send_paced(&mut socket, &registry, &id, pacer, &mut rx, next).await
                    }
                }
            }
            if !open {
                break;
            }
        }
    }
    registry.detach(&id, client);
}

/// Messages a client may send on `/v0/events`, so one connection can drive
/// every session in order.
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum EventsClientMessage {
    Input {
        id: String,
        data: String,
        /// A fleet device's session (relayed); absent for this core's own.
        #[serde(default)]
        device: Option<String>,
    },
    Resize {
        id: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        device: Option<String>,
    },
}

#[derive(Deserialize)]
struct EventsQuery {
    /// `1` includes every session's output (for clients mirroring all
    /// sessions), `first` only the first output of each.
    #[serde(default)]
    output: Option<String>,
}

/// Which `output` events a connection to `/v0/events` gets.
#[derive(Clone, Copy, PartialEq)]
enum OutputEvents {
    None,
    First,
    All,
}

async fn events(
    State(state): State<AppState>,
    Query(query): Query<EventsQuery>,
    caller: Option<Extension<Caller>>,
    ws: WebSocketUpgrade,
) -> Response {
    let output = match query.output.as_deref() {
        Some("1" | "true") => OutputEvents::All,
        Some("first") => OutputEvents::First,
        _ => OutputEvents::None,
    };
    let revocation = Revocation::new(&state.clients, caller.as_deref());
    ws.on_upgrade(move |socket| events_loop(socket, state, output, revocation))
}

async fn events_loop(
    mut socket: WebSocket,
    state: AppState,
    output: OutputEvents,
    mut revocation: Revocation,
) {
    let registry = Arc::clone(&state.registry);
    let relays = Arc::clone(&state.relays);
    let own_id = state.config.get().device_id;
    let client: ClientId = registry.connect();
    // This core's sessions this connection sized, to hand their size back on close
    let mut sized = HashSet::new();
    // Sessions whose first output this connection has had
    let mut started: HashSet<Arc<str>> = HashSet::new();
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
            () = revocation.revoked() => break,
            event = rx.recv() => match event {
                Ok(Event::Output { .. }) if output == OutputEvents::None => {}
                Ok(Event::Output { ref id, .. }) if output == OutputEvents::First && !started.insert(Arc::clone(id)) => {}
                Ok(Event::DeviceEvent { ref event, .. }) if output != OutputEvents::All && event["type"] == "output" => {}
                Ok(event) => {
                    if let Event::SessionRemoved { id } = &event {
                        started.remove(id);
                    }
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
                    Ok(EventsClientMessage::Input { device: Some(device), id, data }) if device != own_id => {
                        relays.send(&device, json!({ "type": "input", "id": id, "data": data }).to_string());
                    }
                    Ok(EventsClientMessage::Resize { device: Some(device), id, cols, rows }) if device != own_id => {
                        relays.send(&device, json!({ "type": "resize", "id": id, "cols": cols, "rows": rows }).to_string());
                    }
                    Ok(EventsClientMessage::Input { id, data, .. }) => { registry.input_from(&id, client, data.into_bytes()); }
                    Ok(EventsClientMessage::Resize { id, cols, rows, .. }) => {
                        if registry.resize_from(&id, client, cols, rows) {
                            sized.insert(id);
                        }
                    }
                    Err(err) => tracing::debug!(%err, "ignoring client message"),
                },
                Some(Ok(Message::Close(_))) | None | Some(Err(_)) => break,
                Some(Ok(_)) => {}
            },
        }
    }
    for id in sized {
        registry.detach(&id, client);
    }
}
