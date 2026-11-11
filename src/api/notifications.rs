//! Phone notifications (see `crate::notify`): a paired client turns its own
//! on or off, and the home core sends them through APNs when an agent waits.

use std::sync::Arc;

use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;

use super::{AppState, error};
use crate::apns::{self, Alert, Sent, Target};
use crate::clients::Caller;
use crate::notify::{self, SessionKey, Waits};

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v0/notifications", get(status).put(set))
        .route("/v0/notifications/test", post(test))
}

/// The paired client making the request; notifications belong to one.
fn client(caller: Option<&Caller>) -> Option<&str> {
    match caller {
        Some(Caller::Client(id)) => Some(id),
        _ => None,
    }
}

fn not_a_client() -> Response {
    error(
        StatusCode::BAD_REQUEST,
        "only a paired client gets notifications",
    )
}

fn describe(state: &AppState, id: &str) -> Response {
    Json(json!({
        "enabled": state.clients.push_target(id).is_some(),
        "available": apns::load(&state.core_dir).is_some(),
    }))
    .into_response()
}

async fn status(State(state): State<AppState>, caller: Option<Extension<Caller>>) -> Response {
    match client(caller.as_deref()) {
        Some(id) => describe(&state, id),
        None => not_a_client(),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Change {
    enabled: bool,
    device_token: Option<String>,
    environment: Option<String>,
    topic: Option<String>,
}

async fn set(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Json(change): Json<Change>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let target = if change.enabled {
        let (Some(device_token), Some(environment), Some(topic)) =
            (change.device_token, change.environment, change.topic)
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "turning notifications on needs deviceToken, environment and topic",
            );
        };
        let target = Target {
            device_token: device_token.to_ascii_lowercase(),
            environment,
            topic,
        };
        if let Err(err) = target.validate() {
            return error(StatusCode::BAD_REQUEST, &format!("{err:#}"));
        }
        Some(target)
    } else {
        None
    };
    match state.clients.set_push(id, target) {
        Ok(()) => {
            tracing::info!(client = %id, on = change.enabled, "notifications changed");
            describe(&state, id)
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Send the client a notification now, to see that it arrives.
async fn test(State(state): State<AppState>, caller: Option<Extension<Caller>>) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let Some(target) = state.clients.push_target(id) else {
        return error(StatusCode::BAD_REQUEST, "notifications are off");
    };
    let Some(key) = apns::load(&state.core_dir) else {
        return error(StatusCode::CONFLICT, "the home core has no APNs key");
    };
    let alert = Alert {
        title: "TUI-DO".into(),
        body: format!(
            "Notifications from {} work. You'll get one when an agent waits for you.",
            state.config.get().device_name
        ),
        link: None,
        thread: None,
    };
    match deliver(&state, &key, &target, &alert).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
    }
}

/// Send one notification, forgetting a device APNs says is gone.
async fn deliver(
    state: &AppState,
    key: &apns::Key,
    target: &Target,
    alert: &Alert,
) -> anyhow::Result<()> {
    match apns::send(key, target, alert).await? {
        Sent::Delivered => Ok(()),
        Sent::Unregistered => {
            state.clients.forget_device_token(&target.device_token)?;
            anyhow::bail!(
                "the phone isn't registered for notifications any more; turn them on again"
            )
        }
    }
}

/// Watch sessions' statuses and notify clients about the ones left calling
/// for someone.
pub fn spawn_notifier(state: AppState) {
    let mut events = state.registry.subscribe();
    tokio::spawn(async move {
        let mut waits = Waits::default();
        loop {
            let next = waits.next();
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) => {
                        let own_id = state.config.get().device_id;
                        if let Some(change) = notify::change(&event, &own_id) {
                            waits.apply(change, tokio::time::Instant::now());
                        }
                    }
                    // Missed changes only cost a notification
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                () = tokio::time::sleep_until(next.unwrap_or_else(tokio::time::Instant::now)), if next.is_some() => {
                    for (key, body) in waits.take_due(tokio::time::Instant::now()) {
                        tokio::spawn(send(state.clone(), key, body));
                    }
                }
            }
        }
    });
}

/// Notify each client with notifications on that doesn't have the session open.
async fn send(state: AppState, key: SessionKey, body: String) {
    let config = state.config.get();
    // Only the home has the clients; a runner's sessions reach it as relayed events
    if !config.is_home() {
        return;
    }
    let targets: Vec<Target> = state
        .clients
        .push_targets()
        .into_iter()
        .filter(|(client, _)| !state.watching.is_open(client, &key))
        .map(|(_, target)| target)
        .collect();
    if targets.is_empty() {
        return;
    }
    let Some(apns_key) = apns::load(&state.core_dir) else {
        tracing::warn!("a phone has notifications on, but this home has no APNs key");
        return;
    };
    let (device, session) = &key;
    let machine = if *device == config.device_id {
        config.device_name.clone()
    } else {
        state
            .fleet
            .device(device)
            .map_or_else(|| device.clone(), |d| d.name)
    };
    let workspace = state.workspace.current();
    let slug = session.clone();
    let task = tokio::task::spawn_blocking(move || {
        workspace.and_then(|ws| ws.get_task(&slug).ok().map(|t| t.task.title))
    })
    .await
    .ok()
    .flatten();
    let alert = Alert {
        title: format!("{} · {machine}", task.as_deref().unwrap_or(session)),
        body,
        link: Some(notify::session_link(device, session)),
        thread: Some(format!("{device}/{session}")),
    };
    let sent = targets
        .iter()
        .map(|target| deliver(&state, &apns_key, target, &alert));
    let mut delivered = 0;
    for result in futures_util::future::join_all(sent).await {
        match result {
            Ok(()) => delivered += 1,
            Err(err) => tracing::warn!(error = %format!("{err:#}"), "couldn't send a notification"),
        }
    }
    tracing::info!(%machine, %session, delivered, "notified about an agent");
}

impl AppState {
    /// While a paired client has a session open, it isn't notified about it.
    pub(super) fn watch(
        &self,
        caller: Option<&Caller>,
        device: &str,
        session: &str,
    ) -> Option<notify::Watch> {
        match caller {
            Some(Caller::Client(id)) => {
                Some(Arc::clone(&self.watching).open(id, (device.to_string(), session.to_string())))
            }
            _ => None,
        }
    }
}
