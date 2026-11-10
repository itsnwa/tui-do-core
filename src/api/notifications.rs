//! Phone notifications (see `crate::notify`): a paired client turns its own
//! on or off, and the home core sends them when an agent waits.

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
use crate::clients::Caller;
use crate::notify::{self, Message, SessionKey, Waits};

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

fn describe(state: &AppState, topic: Option<String>) -> Response {
    Json(json!({
        "enabled": topic.is_some(),
        "server": state.config.get().ntfy_server(),
        "topic": topic,
    }))
    .into_response()
}

async fn status(State(state): State<AppState>, caller: Option<Extension<Caller>>) -> Response {
    match client(caller.as_deref()) {
        Some(id) => describe(&state, state.clients.ntfy_topic(id)),
        None => not_a_client(),
    }
}

#[derive(Deserialize)]
struct Change {
    enabled: bool,
}

async fn set(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Json(change): Json<Change>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    match state.clients.set_notifications(id, change.enabled) {
        Ok(topic) => {
            tracing::info!(client = %id, on = change.enabled, "notifications changed");
            describe(&state, topic)
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Send the client a notification now, to see that it arrives.
async fn test(State(state): State<AppState>, caller: Option<Extension<Caller>>) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let Some(topic) = state.clients.ntfy_topic(id) else {
        return error(StatusCode::BAD_REQUEST, "notifications are off");
    };
    let config = state.config.get();
    let message = Message {
        title: "TUI-DO".into(),
        body: format!(
            "Notifications from {} work. You'll get one when an agent waits for you.",
            config.device_name
        ),
        click: None,
    };
    match notify::publish(&config.ntfy_server(), &topic, &message).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(err) => error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
    }
}

/// Watch agent states and notify clients about sessions left waiting.
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
                    for (key, detail) in waits.take_due(tokio::time::Instant::now()) {
                        tokio::spawn(send(state.clone(), key, detail));
                    }
                }
            }
        }
    });
}

/// Notify each client with notifications on that doesn't have the session open.
async fn send(state: AppState, key: SessionKey, detail: Option<String>) {
    let config = state.config.get();
    // Only the home has the clients; a runner's sessions reach it as relayed events
    if !config.is_home() {
        return;
    }
    let topics: Vec<String> = state
        .clients
        .ntfy_topics()
        .into_iter()
        .filter(|(client, _)| !state.watching.is_open(client, &key))
        .map(|(_, topic)| topic)
        .collect();
    if topics.is_empty() {
        return;
    }
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
    let message = Message {
        title: format!("{} · {machine}", task.as_deref().unwrap_or(session)),
        body: detail.unwrap_or_else(|| "Waiting for input".into()),
        click: Some(notify::session_link(device, session)),
    };
    let server = config.ntfy_server();
    let sent = topics
        .iter()
        .map(|topic| notify::publish(&server, topic, &message));
    let mut delivered = 0;
    for result in futures_util::future::join_all(sent).await {
        match result {
            Ok(()) => delivered += 1,
            Err(err) => tracing::warn!(error = %format!("{err:#}"), "couldn't send a notification"),
        }
    }
    tracing::info!(%machine, %session, delivered, "notified about a waiting agent");
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
