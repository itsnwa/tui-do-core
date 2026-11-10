//! The home role: where it is (`GET /v0/home`), the link a home gives each
//! device (`PUT /v0/fleet/home`), the workspace copies it keeps on them
//! (`/v0/replica`), and moving it to another core (`POST /v0/fleet/move`,
//! which hands everything to the target's `POST /v0/fleet/adopt`).
//!
//! A move: the target's copy of the workspace is brought up to date, the
//! home stops taking writes (and waits for those under way), the copy is
//! brought up to date once more, and the target takes the fleet (with the
//! old home in it), the paired clients, the task-session links and the
//! handoff records. It links every device to itself; the old home becomes a
//! runner with a read-only copy.
//!
//! Whenever a core that still holds a fleet takes a link from a newer home,
//! it gives the home role away (`give_away`), so a move whose answer got lost
//! still finishes once the new home reaches the old one. Until it's sure,
//! the old home takes no writes. The app's token goes in that answer too; a
//! Mac that never got it is given one when the new home next reaches it.

use std::sync::Arc;
use std::time::Duration;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::broadcast::error::RecvError;

use super::{AppState, error};
use crate::config::HomeLink;
use crate::fleet::{Device, FleetRecord};
use crate::handoff::Record;
use crate::links::SessionLink;
use crate::session::Event;

/// A replica batch is at most 16 MB of files, unless one file is larger.
const MAX_REPLICA_BATCH: usize = 2 * 1024 * 1024 * 1024;
/// How long the home waits after the last workspace change before pushing it.
const PUSH_DELAY: Duration = Duration::from_secs(3);
/// How long a move waits for writes under way to finish.
const WRITES_WAIT: Duration = Duration::from_secs(30);

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/home", get(get_home))
        .route("/v0/fleet/home", put(set_home))
        .route("/v0/fleet/move", post(move_home))
        .route("/v0/fleet/adopt", post(adopt))
        .route(
            "/v0/replica",
            get(replica_manifest)
                .post(replica_receive)
                .layer(DefaultBodyLimit::max(MAX_REPLICA_BATCH)),
        )
}

fn platform() -> String {
    format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH)
}

/// This core's address over Tailscale, turning remote access on for it.
async fn own_address(state: &AppState) -> anyhow::Result<String> {
    if !state.config.get().remote_access {
        state.config.update(|c| c.remote_access = true)?;
    }
    let ip = crate::net::tailscale_ipv4()
        .await
        .ok_or_else(|| anyhow::anyhow!("Tailscale isn't running here"))?;
    let port = state.local_endpoint.rsplit(':').next().unwrap_or("47821");
    Ok(format!("http://{ip}:{port}"))
}

/// Where the home is, as `/v0/health` and `/v0/home` report it.
pub(super) fn home_summary(state: &AppState) -> Value {
    let config = state.config.get();
    match &config.home {
        None => json!({
            "id": config.device_id,
            "name": config.device_name,
            "generation": state.fleet.generation(),
        }),
        Some(link) => json!({
            "id": link.id,
            "name": link.name,
            "address": link.address,
            "generation": link.generation,
        }),
    }
}

/// `{ self, id, name, address, generation, clientToken }`: where the home is,
/// and on the machine the home moved away from, the app's token for it.
async fn get_home(State(state): State<AppState>) -> Response {
    let config = state.config.get();
    let mut home = home_summary(&state);
    home["self"] = config.is_home().into();
    if let Some(token) = config.home.and_then(|link| link.client_token) {
        home["clientToken"] = token.into();
    }
    Json(home).into_response()
}

/// Give a device a home link: this core's address, the device's MCP-only
/// token, the generation, and (for the old home) a client token.
pub(super) async fn link_home(
    state: &AppState,
    device: &Device,
    client_token: Option<String>,
) -> anyhow::Result<()> {
    let config = state.config.get();
    let link = HomeLink {
        address: own_address(state).await?,
        token: device.mcp_token.clone(),
        id: config.device_id,
        name: config.device_name,
        generation: state.fleet.generation(),
        client_token,
    };
    let res = state
        .fleet
        .client()
        .put(format!("{}/v0/fleet/home", device.address))
        .bearer_auth(&device.token)
        .json(&link)
        .timeout(Duration::from_secs(10))
        .send()
        .await?;
    anyhow::ensure!(
        res.status().is_success(),
        "the device answered {}",
        res.status()
    );
    Ok(())
}

/// The token for the app on a Mac the home moved away from. The same one
/// while this core is the home: the answer to a move and the link that
/// follows it can reach the Mac in either order.
fn app_token(state: &AppState, device: &Device) -> anyhow::Result<String> {
    let mut tokens = state.app_tokens.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(token) = tokens.get(&device.id) {
        return Ok(token.clone());
    }
    let (client, token) = state.clients.create_app(&device.name)?;
    tracing::info!(client = %client.id, "paired the old home's app");
    tokens.insert(device.id.clone(), token.clone());
    Ok(token)
}

/// A token for a Mac the home moved away from that doesn't have one, as when
/// the answer to the move got lost.
async fn missing_app_token(state: &AppState, device: &Device) -> Option<String> {
    if !device.platform.starts_with("macos") || !state.clients.has_app(&device.name) {
        return None;
    }
    let home = device_home(state.fleet.client(), device).await?;
    if !home["clientToken"].is_null() {
        return None;
    }
    app_token(state, device)
        .inspect_err(|err| tracing::warn!(device = %device.name, error = %format!("{err:#}"), "couldn't pair the app"))
        .ok()
}

/// Take a home link: this core is a runner of that home from now on. A link
/// from an older home than the one this core knows is refused. A core that
/// was the home gives the role away.
async fn take_link(state: &AppState, link: HomeLink) -> Result<(), (StatusCode, String)> {
    // The answer to a move and the new home's own link arrive close together
    let _linking = state.linking.lock().await;
    let config = state.config.get();
    let was_home = config.is_home() && !state.fleet.devices().is_empty();
    let known = match &config.home {
        Some(current) => current.generation,
        None if !was_home => 0,
        // A home with devices only gives the role away to a newer home
        None => state.fleet.generation() + 1,
    };
    if link.generation < known {
        return Err((
            StatusCode::CONFLICT,
            "that home is older than the one this machine knows".into(),
        ));
    }
    if was_home {
        give_away(state, link.generation)
            .await
            .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    }
    let changed = config.home.as_ref().is_none_or(|current| {
        current.id != link.id
            || current.address != link.address
            || (current.client_token.is_none() && link.client_token.is_some())
    });
    state
        .config
        .update(|c| {
            // A link without a client token keeps the one this machine has
            let client_token = link
                .client_token
                .clone()
                .or_else(|| c.home.as_ref().and_then(|h| h.client_token.clone()));
            c.home = Some(HomeLink {
                client_token,
                ..link.clone()
            });
        })
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, format!("{err:#}")))?;
    if was_home {
        // Writes are refused from here on (this core is a runner)
        state.workspace.freeze(false);
    }
    if changed {
        tracing::info!(home = %link.name, address = %link.address, generation = link.generation, "linked to a home");
        let _ = state.registry.sender().send(Event::HomeChanged {
            home: link.id,
            generation: link.generation,
        });
    }
    Ok(())
}

/// This core stops being the home: its folder becomes a copy (keeping what
/// was in it), and the fleet, paired clients, session links and handoff
/// records, which the new home has, are cleared here.
async fn give_away(state: &AppState, generation: u64) -> anyhow::Result<()> {
    let (core_dir, folder) = (Arc::clone(&state.core_dir), state.workspace.folder());
    tokio::task::spawn_blocking(move || crate::replica::mark_moved(&core_dir, &folder)).await??;
    let old_devices = state.fleet.devices();
    state.fleet.replace(FleetRecord {
        generation,
        code_device: None,
        devices: Vec::new(),
    })?;
    for device in old_devices {
        state.relays.stop(&device.id);
    }
    state.clients.replace(crate::clients::Record::default())?;
    state
        .app_tokens
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .clear();
    state.sessions.replace(&[])?;
    state.handoffs.replace_records(&[])?;
    tracing::info!(generation, "gave the home away");
    Ok(())
}

async fn set_home(State(state): State<AppState>, Json(link): Json<HomeLink>) -> Response {
    if !(link.address.starts_with("http://") || link.address.starts_with("https://")) {
        return error(StatusCode::BAD_REQUEST, "address must be an http(s) URL");
    }
    match take_link(&state, link).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err((status, message)) => error(status, &message),
    }
}

// --- replicas ---

/// Who a push is from, for the device to check it's its home.
fn sender(state: &AppState) -> crate::replica::Sender {
    crate::replica::Sender {
        id: state.config.get().device_id,
        generation: state.fleet.generation(),
    }
}

async fn push_to(state: &AppState, device: &Device) -> anyhow::Result<usize> {
    state
        .replicas
        .push(
            state.fleet.client(),
            device,
            &state.workspace.folder(),
            &sender(state),
        )
        .await
}

async fn replica_manifest(State(state): State<AppState>) -> Response {
    let folder = state.workspace.folder();
    match tokio::task::spawn_blocking(move || crate::replica::manifest(&folder)).await {
        Ok(manifest) => Json(manifest).into_response(),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

#[derive(Deserialize)]
struct BatchFrom {
    home: String,
    generation: u64,
}

/// A batch of the home's workspace, for this core's copy. Only from this
/// core's home, and only into a folder that's a copy.
async fn replica_receive(
    State(state): State<AppState>,
    Query(from): Query<BatchFrom>,
    body: Bytes,
) -> Response {
    let config = state.config.get();
    let Some(home) = config.home else {
        return error(
            StatusCode::CONFLICT,
            "this machine is the home; its workspace isn't a copy",
        );
    };
    if home.id != from.home || home.generation != from.generation {
        return error(
            StatusCode::CONFLICT,
            "that isn't this machine's home (or not as of now)",
        );
    }
    let folder = state.workspace.folder();
    let default = state.workspace.default_folder().to_path_buf();
    let core_dir = Arc::clone(&state.core_dir);
    let applied = tokio::task::spawn_blocking(move || {
        crate::replica::accepts(&core_dir, &folder, &default)?;
        crate::replica::apply(&folder, &body)
    })
    .await;
    match applied {
        Ok(Ok(changed)) => {
            if let Err(err) = state.workspace.ensure_open() {
                tracing::warn!(%err, "couldn't open the workspace copy");
            }
            Json(json!({ "changed": changed })).into_response()
        }
        Ok(Err(err)) => error(StatusCode::CONFLICT, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// On the home, for one device that came online (or every device): link it
/// here, so devices that were offline during a move follow it, and bring its
/// copy up to date.
fn welcome(state: &AppState, device: Device) {
    let state = state.clone();
    tokio::spawn(async move {
        if !state.config.get().is_home() {
            return;
        }
        let client_token = missing_app_token(&state, &device).await;
        if let Err(err) = link_home(&state, &device, client_token).await {
            tracing::warn!(device = %device.name, error = %format!("{err:#}"), "couldn't give the device a home link");
        }
        if state.workspace.current().is_some()
            && !state.workspace.is_frozen()
            && let Err(err) = push_to(&state, &device).await
        {
            tracing::debug!(device = %device.name, error = %format!("{err:#}"), "couldn't update the workspace copy");
        }
    });
}

/// Bring every device's copy up to date (offline ones are skipped).
async fn push_all(state: &AppState) {
    let pushes = state.fleet.devices().into_iter().map(|device| async move {
        if let Err(err) = push_to(state, &device).await {
            tracing::debug!(device = %device.name, error = %format!("{err:#}"), "couldn't update the workspace copy");
        }
    });
    futures_util::future::join_all(pushes).await;
}

/// On the home: welcome each device that comes online, and push workspace
/// changes to every device shortly after they happen. Pushes run on their
/// own, so this keeps up with the events (which carry terminal output too).
pub fn spawn_background(state: AppState) {
    let mut events = state.registry.subscribe();
    tokio::spawn(async move {
        let pushing = Arc::new(tokio::sync::Mutex::new(()));
        let mut pending = false;
        let mut deadline = tokio::time::Instant::now();
        loop {
            let event = tokio::select! {
                event = events.recv() => event,
                () = tokio::time::sleep_until(deadline), if pending => {
                    pending = false;
                    if state.config.get().is_home() && !state.workspace.is_frozen() {
                        // One round at a time; changes during it wait for the next
                        match Arc::clone(&pushing).try_lock_owned() {
                            Ok(round) => {
                                let state = state.clone();
                                tokio::spawn(async move {
                                    push_all(&state).await;
                                    drop(round);
                                });
                            }
                            Err(_) => {
                                pending = true;
                                deadline = tokio::time::Instant::now() + PUSH_DELAY;
                            }
                        }
                    }
                    continue;
                }
            };
            match event {
                Ok(Event::DeviceStatus {
                    device,
                    online: true,
                }) => {
                    if let Some(device) = state.fleet.device(&device) {
                        welcome(&state, device);
                    }
                }
                Ok(Event::WorkspaceChanged { .. }) => {
                    if !pending {
                        pending = true;
                        deadline = tokio::time::Instant::now() + PUSH_DELAY;
                    }
                }
                // Missed events may have been devices coming online
                Err(RecvError::Lagged(_)) => {
                    for device in state.fleet.devices() {
                        welcome(&state, device);
                    }
                }
                Err(RecvError::Closed) => return,
                Ok(_) => {}
            }
        }
    });
}

// --- moving the home ---

/// Everything a home holds besides the workspace, handed to the new home.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Handover {
    /// The new fleet: the old home is in it, the new home isn't.
    fleet: FleetRecord,
    clients: crate::clients::Record,
    sessions: Vec<(String, SessionLink)>,
    handoffs: Vec<(String, Record)>,
    /// The key phone notifications are sent with (homes before 0.3.11 have none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    apns: Option<crate::apns::Key>,
    /// The old home's device id.
    from: String,
}

#[derive(Deserialize)]
struct MoveBody {
    device: String,
}

async fn move_home(State(state): State<AppState>, Json(body): Json<MoveBody>) -> Response {
    if !state.config.get().is_home() {
        return error(
            StatusCode::CONFLICT,
            "only the home can move the home; ask it instead",
        );
    }
    let Some(target) = state.fleet.device(&body.device) else {
        return error(StatusCode::NOT_FOUND, "no such machine");
    };
    let Ok(moving) = Arc::clone(&state.moving).try_lock_owned() else {
        return error(StatusCode::CONFLICT, "the home is already moving");
    };
    if state.workspace.current().is_none() {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    }
    // On its own task, so a client that stops waiting can't cut it short
    let task = tokio::spawn(async move {
        let result = move_to(&state, &target).await;
        drop(moving);
        if let Err(err) = &result {
            tracing::warn!(target = %target.name, error = %format!("{err:#}"), "moving the home failed");
        }
        result
    });
    match task.await {
        Ok(Ok(home)) => Json(home).into_response(),
        Ok(Err(err)) => error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

async fn move_to(state: &AppState, target: &Device) -> anyhow::Result<Value> {
    let client = state.fleet.client();
    let own = state.config.get();
    let address = own_address(state).await?;
    let root = state.workspace.folder();
    let complete = {
        let root = root.clone();
        tokio::task::spawn_blocking(move || crate::replica::manifest(&root).complete).await?
    };
    anyhow::ensure!(
        complete,
        "iCloud hasn't downloaded all of the task files yet; try again once it has"
    );
    tracing::info!(target = %target.name, "moving the home");

    // Most of the workspace travels while it's still in use
    push_to(state, target).await?;
    state.workspace.freeze(true);
    let Some(writes) = state.workspace.hold_writes(WRITES_WAIT).await else {
        state.workspace.freeze(false);
        anyhow::bail!("changes to the tasks are still being made; try again in a moment");
    };
    let copied = async {
        push_to(state, target).await?;
        // Nothing may be missing on the other side
        anyhow::ensure!(
            push_to(state, target).await? == 0,
            "the workspace kept changing while it was copied; try again"
        );
        Ok(())
    };
    if let Err(err) = copied.await {
        state.workspace.freeze(false);
        return Err(err);
    }

    let current = state.fleet.record();
    let generation = current.generation + 1;
    let mut devices: Vec<Device> = current
        .devices
        .into_iter()
        .filter(|d| d.id != target.id)
        .collect();
    devices.push(Device {
        id: own.device_id.clone(),
        name: own.device_name.clone(),
        address,
        token: state.token.to_string(),
        platform: platform(),
        added_at: crate::workspace::dates::now_iso(),
        ssh_host: None,
        mcp_token: uuid::Uuid::new_v4().simple().to_string(),
    });
    let handover = Handover {
        fleet: FleetRecord {
            generation,
            // The projects' folders stay where they are
            code_device: Some(current.code_device.unwrap_or_else(|| own.device_id.clone())),
            devices,
        },
        clients: state.clients.record(),
        sessions: state.sessions.all(),
        handoffs: state
            .handoffs
            .records()
            .into_iter()
            .map(|(task, mut record)| {
                if record.source.is_empty() {
                    record.source.clone_from(&own.device_id);
                }
                (task, record)
            })
            .collect(),
        apns: crate::apns::load(&state.core_dir),
        from: own.device_id.clone(),
    };
    let adopted = match send_adopt(client, target, &handover).await {
        Ok(link) => Ok(link),
        // Unclear whether it took over: ask it, and ask again for the link
        // (taking over twice is harmless) if it did
        Err(err) => match home_of(client, target).await {
            Some((true, at)) if at == generation => send_adopt(client, target, &handover).await,
            Some(_) => {
                state.workspace.freeze(false);
                return Err(err);
            }
            None => Err(err),
        },
    };
    drop(writes);
    let link = adopted.map_err(|err| {
        // Writes stay paused: the target may be the home now, and it gives
        // this core a link (which ends the pause) once it reaches it
        anyhow::anyhow!(
            "{err:#}. It isn't clear whether {} took the home, so this machine takes no changes until {} reaches it (or this core restarts)",
            target.name,
            target.name
        )
    })?;
    take_link(state, link)
        .await
        .map_err(|(_, message)| anyhow::anyhow!(message))?;
    tracing::info!(home = %target.name, generation, "the home moved");
    Ok(json!({
        "home": { "id": target.id, "name": target.name, "address": target.address },
        "generation": generation,
    }))
}

async fn send_adopt(
    client: &reqwest::Client,
    target: &Device,
    handover: &Handover,
) -> anyhow::Result<HomeLink> {
    let res = client
        .post(format!("{}/v0/fleet/adopt", target.address))
        .bearer_auth(&target.token)
        .json(handover)
        .timeout(Duration::from_secs(120))
        .send()
        .await
        .map_err(|err| anyhow::anyhow!("{} didn't answer: {err}", target.name))?;
    if res.status() == StatusCode::NOT_FOUND {
        anyhow::bail!(
            "{}'s core is too old to be the home; update it first",
            target.name
        );
    }
    if !res.status().is_success() {
        let text = res.text().await.unwrap_or_default();
        anyhow::bail!("{} didn't take the home: {text}", target.name);
    }
    Ok(res.json().await?)
}

/// A device's `GET /v0/home` (`None` if it can't be asked).
async fn device_home(client: &reqwest::Client, device: &Device) -> Option<Value> {
    client
        .get(format!("{}/v0/home", device.address))
        .bearer_auth(&device.token)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()
}

/// Whether a device says it's the home, and its generation (`None` if it
/// can't be asked).
async fn home_of(client: &reqwest::Client, device: &Device) -> Option<(bool, u64)> {
    let home = device_home(client, device).await?;
    Some((home["self"] == true, home["generation"].as_u64()?))
}

/// Take the home role from the old home (see the module docs). Answers with
/// the old home's link to this core.
async fn adopt(State(state): State<AppState>, Json(handover): Json<Handover>) -> Response {
    match adopt_inner(&state, handover).await {
        Ok(link) => Json(link).into_response(),
        Err(err) => {
            tracing::warn!(error = %format!("{err:#}"), "taking the home failed");
            error(StatusCode::BAD_REQUEST, &format!("{err:#}"))
        }
    }
}

async fn adopt_inner(state: &AppState, handover: Handover) -> anyhow::Result<HomeLink> {
    let config = state.config.get();
    let Some(old_home) = handover
        .fleet
        .devices
        .iter()
        .find(|d| d.id == handover.from)
        .cloned()
    else {
        anyhow::bail!("the old home isn't in the fleet it handed over");
    };
    // Everything that can fail comes before anything changes
    let address = own_address(state).await?;
    let already = config.is_home()
        && state.fleet.generation() == handover.fleet.generation
        && state.fleet.device(&handover.from).is_some();
    if !already {
        if config.is_home() && !state.fleet.devices().is_empty() {
            anyhow::bail!("this machine is already a home with its own fleet");
        }
        if let Some(link) = &config.home
            && handover.fleet.generation <= link.generation
        {
            anyhow::bail!("this machine knows a newer home");
        }
        let folder = state.workspace.folder();
        let has_tasks = tokio::task::spawn_blocking(move || {
            crate::replica::manifest(&folder)
                .files
                .keys()
                .any(|p| p.starts_with("tasks/"))
        })
        .await?;
        anyhow::ensure!(has_tasks, "the workspace copy here is empty");
        take_over(state, handover)?;
    }

    // The app runs on Macs: one the home left keeps reaching it as a client
    let client_token = if old_home.platform.starts_with("macos") {
        Some(app_token(state, &old_home)?)
    } else {
        None
    };
    let config = state.config.get();
    Ok(HomeLink {
        address,
        token: state
            .fleet
            .device(&old_home.id)
            .map_or(old_home.mcp_token, |d| d.mcp_token),
        id: config.device_id,
        name: config.device_name,
        generation: state.fleet.generation(),
        client_token,
    })
}

/// Become the home with what the old home handed over.
fn take_over(state: &AppState, handover: Handover) -> anyhow::Result<()> {
    let config = state.config.get();
    let own_id = config.device_id.clone();
    let folder = state.workspace.folder();
    let mut fleet = handover.fleet;
    fleet.devices.retain(|d| d.id != own_id);
    if fleet.code_device.as_deref() == Some(own_id.as_str()) {
        fleet.code_device = None;
    }
    state.fleet.replace(fleet)?;
    state.clients.replace(handover.clients)?;
    // This machine's app reaches the home directly now; its client token goes
    if let Some(token) = config.home.as_ref().and_then(|h| h.client_token.as_deref()) {
        let _ = state.clients.revoke_token(token);
    }
    state.sessions.replace(&handover.sessions)?;
    state.handoffs.replace_records(&handover.handoffs)?;
    if let Some(key) = &handover.apns {
        crate::apns::save(&state.core_dir, key)?;
    }
    state.config.update(|c| {
        c.home = None;
        c.workspace_path = Some(folder.clone());
        c.remote_access = true;
    })?;
    // The folder isn't a copy any more; the next move away keeps a backup again
    crate::replica::unmark(&state.core_dir);
    state.workspace.ensure_open()?;
    let generation = state.fleet.generation();
    let _ = state.registry.sender().send(Event::HomeChanged {
        home: own_id,
        generation,
    });
    // Relaying brings each device online, which links it here
    for device in state.fleet.devices() {
        state.relays.start(device);
    }
    tracing::info!(generation, "took the home");
    Ok(())
}
