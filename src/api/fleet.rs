//! Fleet routes on the home core: the device list, adding and removing
//! devices, sessions across the fleet, and `/v0/devices/{id}/…`, which
//! forwards any request (including WebSockets) to that device with its token.

use axum::Json;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Extension, FromRequestParts, Path, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio_tungstenite::tungstenite;

use super::{AppState, error};
use crate::clients::{Caller, Revocation};
use crate::fleet::Device;
use crate::install::InstallRequest;

/// Largest request body forwarded to a device.
const MAX_BODY: usize = 32 * 1024 * 1024;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/fleet", get(get_fleet))
        .route("/v0/fleet/devices", post(add_device))
        .route("/v0/fleet/install", post(install_device))
        .route("/v0/fleet/devices/{id}", delete(remove_device))
        .route("/v0/fleet/devices/{id}/update", post(update_device))
        .route("/v0/fleet/sessions", get(fleet_sessions))
        .route("/v0/devices/{id}/{*rest}", any(proxy))
}

async fn get_fleet(State(state): State<AppState>) -> Response {
    let config = state.config.get();
    Json(json!({
        "generation": state.fleet.generation(),
        "home": {
            "id": config.device_id,
            "name": config.device_name,
            "platform": format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            "version": env!("CARGO_PKG_VERSION"),
        },
        "codeDevice": state.fleet.code_device(&config.device_id),
        "devices": state.fleet.statuses().await,
    }))
    .into_response()
}

#[derive(Deserialize)]
struct AddDevice {
    /// Base URL of the device's core, e.g. `http://100.79.154.110:47821`.
    address: String,
    token: String,
}

async fn add_device(State(state): State<AppState>, Json(body): Json<AddDevice>) -> Response {
    if !(body.address.starts_with("http://") || body.address.starts_with("https://")) {
        return error(StatusCode::BAD_REQUEST, "address must be an http(s) URL");
    }
    let own_id = state.config.get().device_id;
    match state
        .fleet
        .add(&body.address, &body.token, &own_id, None)
        .await
    {
        Ok(device) => {
            tracing::info!(id = %device.id, name = %device.name, address = %device.address, "device added");
            joined(&state, &device).await;
            let status = state
                .fleet
                .statuses()
                .await
                .into_iter()
                .find(|d| d.id == device.id);
            Json(status).into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}

/// Install a core on another machine over ssh and add it (takes a minute;
/// progress goes out as `installProgress` events).
async fn install_device(
    State(state): State<AppState>,
    Json(req): Json<InstallRequest>,
) -> Response {
    let own_id = state.config.get().device_id;
    let events = state.registry.sender();
    let instructions = req.instructions.clone();
    match crate::install::install(req, &state.fleet, &own_id, &events).await {
        Ok(device) => {
            joined(&state, &device).await;
            if let Some(instructions) = instructions {
                setup_agents_on(&state, &device, &instructions).await;
            }
            let status = state
                .fleet
                .statuses()
                .await
                .into_iter()
                .find(|d| d.id == device.id);
            Json(status).into_response()
        }
        Err(err) => error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct UpdateDevice {
    /// Release to install; defaults to the latest.
    version: Option<String>,
    /// Workflow instructions for the coding agents there, set up again afterwards.
    instructions: Option<String>,
}

/// Update a device's core to a release: it updates itself and restarts, and
/// this waits until it's back on the new version. A device too old to update
/// itself is reinstalled over the ssh host it was installed through.
async fn update_device(
    State(state): State<AppState>,
    Path(id): Path<String>,
    body: Option<Json<UpdateDevice>>,
) -> Response {
    let Some(device) = state.fleet.device(&id) else {
        return error(StatusCode::NOT_FOUND, "no such device");
    };
    let body = body.map(|Json(b)| b).unwrap_or_default();
    let version = match body.version {
        Some(version) => version,
        None => match crate::update::latest().await {
            Ok(version) => version,
            Err(err) => return error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
        },
    };
    let sent = state
        .fleet
        .client()
        .post(format!("{}/v0/update", device.address))
        .bearer_auth(&device.token)
        .json(&json!({ "version": version }))
        .timeout(std::time::Duration::from_secs(600))
        .send()
        .await;
    let res = match sent {
        Ok(res) => res,
        Err(err) => {
            return error(
                StatusCode::BAD_GATEWAY,
                &format!("{} isn't reachable: {err}", device.name),
            );
        }
    };
    if res.status() == StatusCode::NOT_FOUND || res.status() == StatusCode::METHOD_NOT_ALLOWED {
        // Older than self-updating: install over ssh instead
        let Some(host) = device.ssh_host.clone() else {
            return error(
                StatusCode::CONFLICT,
                "this machine's core is too old to update itself and wasn't added over ssh",
            );
        };
        return install_device(
            State(state),
            Json(InstallRequest {
                host,
                version: Some(version),
                instructions: body.instructions,
                ..InstallRequest::default()
            }),
        )
        .await;
    }
    if !res.status().is_success() {
        let text = res.text().await.unwrap_or_default();
        return error(
            StatusCode::BAD_GATEWAY,
            &format!("{} couldn't update: {text}", device.name),
        );
    }
    // Wait for it to come back on the new version
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(90);
    let back = loop {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let running = state
            .fleet
            .statuses()
            .await
            .into_iter()
            .find(|d| d.id == device.id)
            .and_then(|d| d.version);
        if running.as_deref() == Some(version.as_str()) {
            break true;
        }
        if let Some(current) = rolled_back_from(&state, &device, &version).await {
            return error(
                StatusCode::BAD_GATEWAY,
                &format!(
                    "{version} didn't start on {}, so it went back to {current}",
                    device.name
                ),
            );
        }
        if std::time::Instant::now() > deadline {
            break false;
        }
    };
    if !back {
        return error(
            StatusCode::GATEWAY_TIMEOUT,
            &format!("{} didn't come back on {version}", device.name),
        );
    }
    tracing::info!(device = %device.name, version, "device updated");
    joined(&state, &device).await;
    if let Some(instructions) = body.instructions {
        setup_agents_on(&state, &device, &instructions).await;
    }
    let status = state
        .fleet
        .statuses()
        .await
        .into_iter()
        .find(|d| d.id == device.id);
    Json(status).into_response()
}

async fn remove_device(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    if state.fleet.code_device(&state.config.get().device_id) == id {
        return error(
            StatusCode::CONFLICT,
            "the projects' folders are on that machine, so it stays in the fleet",
        );
    }
    match state.fleet.remove(&id) {
        Ok(true) => {
            state.relays.stop(&id);
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such device"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// The version a device is back on, if it gave up on `version` and rolled back.
async fn rolled_back_from(state: &AppState, device: &Device, version: &str) -> Option<String> {
    let status: serde_json::Value = state
        .fleet
        .client()
        .get(format!("{}/v0/update", device.address))
        .bearer_auth(&device.token)
        .timeout(std::time::Duration::from_secs(5))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    (status["rolledBack"]["version"] == version)
        .then(|| status["current"].as_str().map(str::to_string))
        .flatten()
}

/// A device joined (or was updated): relay its events, and tell it how to
/// reach this core for MCP tool calls. That needs this core reachable over
/// Tailscale, so remote access is turned on here too.
async fn joined(state: &AppState, device: &Device) {
    state.relays.start(device.clone());
    if let Err(err) = super::home::link_home(state, device, None).await {
        tracing::warn!(device = %device.name, error = %format!("{err:#}"), "couldn't give the device a home link");
    }
}

/// Have the device register the TUI-DO tools with its coding agents.
async fn setup_agents_on(state: &AppState, device: &Device, instructions: &str) {
    let sent = state
        .fleet
        .client()
        .put(format!("{}/v0/agents/setup", device.address))
        .bearer_auth(&device.token)
        .json(&json!({ "instructions": instructions }))
        .send()
        .await;
    match sent {
        Ok(res) if res.status().is_success() => {
            let done = res.text().await.unwrap_or_default();
            tracing::info!(device = %device.name, agents = %done, "agents set up");
        }
        Ok(res) => {
            tracing::warn!(device = %device.name, status = %res.status(), "agent setup failed")
        }
        Err(err) => tracing::warn!(device = %device.name, %err, "agent setup failed"),
    }
}

/// Sessions on this core and every reachable device, each with a `device` id.
async fn fleet_sessions(State(state): State<AppState>) -> Response {
    let own_id = state.config.get().device_id;
    let mut all: Vec<serde_json::Value> = state
        .registry
        .list()
        .into_iter()
        .filter_map(|s| serde_json::to_value(s).ok())
        .map(|mut s| {
            s["device"] = own_id.clone().into();
            s
        })
        .collect();
    let remote = state.fleet.devices().into_iter().map(|device| {
        let client = state.fleet.client().clone();
        async move {
            let res = client
                .get(format!("{}/v0/sessions", device.address))
                .bearer_auth(&device.token)
                .timeout(std::time::Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            let sessions: Vec<serde_json::Value> = res.json().await.ok()?;
            Some(sessions.into_iter().map(move |mut s| {
                s["device"] = device.id.clone().into();
                s
            }))
        }
    });
    for sessions in futures_util::future::join_all(remote)
        .await
        .into_iter()
        .flatten()
    {
        all.extend(sessions);
    }
    Json(all).into_response()
}

/// The device a request is for: a fleet device, or this core itself.
pub(super) fn target(state: &AppState, id: &str) -> Option<Device> {
    let config = state.config.get();
    if id == config.device_id {
        return Some(Device {
            id: config.device_id,
            name: config.device_name,
            address: state.local_endpoint.to_string(),
            token: state.token.to_string(),
            platform: String::new(),
            added_at: String::new(),
            ssh_host: None,
            mcp_token: String::new(),
        });
    }
    state.fleet.device(id)
}

async fn proxy(
    State(state): State<AppState>,
    Path((id, rest)): Path<(String, String)>,
    caller: Option<Extension<Caller>>,
    req: Request,
) -> Response {
    let Some(device) = target(&state, &id) else {
        return error(StatusCode::NOT_FOUND, "no such device");
    };
    if matches!(caller.as_deref(), Some(Caller::Client(_))) && reaches_owner_only(&device, &rest) {
        return error(
            StatusCode::FORBIDDEN,
            "only the core's own token can use this",
        );
    }
    // The client's own token is for this core; it's never passed on
    let query: Vec<(String, String)> = req
        .uri()
        .query()
        .map(|q| {
            url_pairs(q)
                .into_iter()
                .filter(|(k, _)| k != "token")
                .collect()
        })
        .unwrap_or_default();
    let is_websocket = req
        .headers()
        .get(header::UPGRADE)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"websocket"));
    if is_websocket {
        let (mut parts, _) = req.into_parts();
        let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(ws) => ws,
            Err(rejection) => return rejection.into_response(),
        };
        let mut query = query;
        query.push(("token".into(), device.token.clone()));
        let url = format!(
            "{}/v0/{rest}?{}",
            device.address.replacen("http", "ws", 1),
            encode_pairs(&query)
        );
        let revocation = Revocation::new(&state.clients, caller.as_deref());
        let watch = attached_session(&rest)
            .and_then(|session| state.watch(caller.as_deref(), &device.id, session));
        return ws.on_upgrade(move |socket| async move {
            bridge(socket, url, revocation).await;
            drop(watch);
        });
    }

    let method = req.method().clone();
    let content_type = req.headers().get(header::CONTENT_TYPE).cloned();
    let body = match axum::body::to_bytes(req.into_body(), MAX_BODY).await {
        Ok(body) => body,
        Err(_) => return error(StatusCode::PAYLOAD_TOO_LARGE, "request body too large"),
    };
    let mut forward = state
        .fleet
        .client()
        .request(method, format!("{}/v0/{rest}", device.address))
        .query(&query)
        .bearer_auth(&device.token)
        .body(body);
    if let Some(ct) = content_type {
        forward = forward.header(header::CONTENT_TYPE, ct);
    }
    match forward.send().await {
        Ok(res) => {
            let status = res.status();
            let content_type = res.headers().get(header::CONTENT_TYPE).cloned();
            let bytes = res.bytes().await.unwrap_or_default();
            let mut response = (status, bytes).into_response();
            if let Some(ct) = content_type {
                response.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            response
        }
        Err(err) => error(
            StatusCode::BAD_GATEWAY,
            &format!("{} isn't reachable: {err}", device.name),
        ),
    }
}

/// Whether forwarding `rest` to a device lands on a route only the core's own
/// token may use. `rest` arrives percent-decoded, and the URL it's sent to is
/// normalized again (`..`, a `?` in it), so this checks where it really goes.
fn reaches_owner_only(device: &Device, rest: &str) -> bool {
    reqwest::Url::parse(&format!("{}/v0/{rest}", device.address))
        .map_or(true, |url| super::clients::owner_only(url.path()))
}

/// Decode `a=1&b=2`, percent-decoding keys and values.
fn url_pairs(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (decode(k), decode(v))
        })
        .collect()
}

fn decode(s: &str) -> String {
    let bytes = s.replace('+', " ").into_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn encode_pairs(pairs: &[(String, String)]) -> String {
    let enc = |s: &str| -> String {
        s.bytes()
            .map(|b| match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    (b as char).to_string()
                }
                _ => format!("%{b:02X}"),
            })
            .collect()
    };
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// The session a forwarded `sessions/{id}/attach` opens.
fn attached_session(rest: &str) -> Option<&str> {
    rest.strip_prefix("sessions/")?
        .strip_suffix("/attach")
        .filter(|id| !id.is_empty() && !id.contains('/'))
}

/// Pass WebSocket messages both ways between a client and a device, until
/// either side closes or the client is revoked.
async fn bridge(client: WebSocket, url: String, mut revocation: Revocation) {
    let upstream = match tokio_tungstenite::connect_async(url.as_str()).await {
        Ok((upstream, _)) => upstream,
        Err(err) => {
            tracing::warn!(%err, "couldn't open the device's WebSocket");
            return;
        }
    };
    let (mut up_tx, mut up_rx) = upstream.split();
    let (mut client_tx, mut client_rx) = client.split();
    let to_device = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let msg = match msg {
                Message::Text(t) => tungstenite::Message::text(t.as_str()),
                Message::Binary(b) => tungstenite::Message::binary(b),
                Message::Ping(p) => tungstenite::Message::Ping(p),
                Message::Pong(p) => tungstenite::Message::Pong(p),
                Message::Close(_) => break,
            };
            if up_tx.send(msg).await.is_err() {
                break;
            }
        }
        let _ = up_tx.close().await;
    };
    let to_client = async {
        while let Some(Ok(msg)) = up_rx.next().await {
            let msg = match msg {
                tungstenite::Message::Text(t) => Message::text(t.as_str()),
                tungstenite::Message::Binary(b) => Message::binary(b),
                tungstenite::Message::Ping(p) => Message::Ping(p),
                tungstenite::Message::Pong(p) => Message::Pong(p),
                tungstenite::Message::Close(_) => break,
                tungstenite::Message::Frame(_) => continue,
            };
            if client_tx.send(msg).await.is_err() {
                break;
            }
        }
        let _ = client_tx.close().await;
    };
    tokio::select! {
        () = to_device => {}
        () = to_client => {}
        () = revocation.revoked() => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_session_attach_should_count_as_opening_it() {
        assert_eq!(attached_session("sessions/26-962/attach"), Some("26-962"));
        assert_eq!(attached_session("sessions/26-962/snapshot"), None);
        assert_eq!(attached_session("sessions//attach"), None);
        assert_eq!(attached_session("sessions/a/b/attach"), None);
        assert_eq!(attached_session("events"), None);
    }

    #[test]
    fn query_should_round_trip() {
        let pairs = url_pairs("project=tui-do&due=2026-11-07&q=a%20b");
        assert_eq!(pairs[2], ("q".into(), "a b".into()));
        assert_eq!(
            encode_pairs(&pairs),
            "project=tui-do&due=2026-11-07&q=a%20b"
        );
    }

    #[test]
    fn client_routes_should_not_be_reachable_through_the_proxy() {
        let device = Device {
            id: "d".into(),
            name: "d".into(),
            address: "http://127.0.0.1:47821".into(),
            token: String::new(),
            platform: String::new(),
            added_at: String::new(),
            ssh_host: None,
            mcp_token: String::new(),
        };
        for rest in [
            "clients",
            "sessions/../clients",
            "clients?x=1",
            "clients/abc",
            "./clients",
        ] {
            assert!(reaches_owner_only(&device, rest), "{rest}");
        }
        assert!(!reaches_owner_only(&device, "sessions/my-task/attach"));
    }

    #[test]
    fn truncated_escape_should_stay_literal() {
        assert_eq!(decode("100%"), "100%");
    }
}
