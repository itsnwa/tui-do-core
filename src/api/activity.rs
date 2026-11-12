//! Each task's activity (see `crate::activity`): `GET /v0/activity`, and the
//! task that keeps the board up to date and tells clients what changed.

use std::time::Duration;

use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use tokio::sync::broadcast::error::RecvError;

use super::AppState;
use crate::activity::TaskRef;
use crate::session::Event;
use crate::workspace::tasks::TaskFilter;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v0/activity", get(list))
}

async fn list(State(state): State<AppState>) -> Response {
    Json(state.activity.all()).into_response()
}

/// Read the tasks again: one of them changed, or the home moved here.
async fn load_tasks(state: &AppState) {
    let workspace = state.workspace.current();
    let tasks = tokio::task::spawn_blocking(move || {
        let tasks = workspace?.list_tasks(&TaskFilter::default()).ok()?;
        let refs = tasks.into_iter().map(|task| TaskRef {
            slug: task.slug,
            state: task.state,
            device: task.device,
        });
        Some(refs.collect::<Vec<_>>())
    })
    .await;
    state
        .activity
        .set_tasks(tasks.ok().flatten().unwrap_or_default());
}

/// Work the activities out again and tell clients the ones that changed.
fn refresh(state: &AppState) {
    let config = state.config.get();
    let changed = if config.is_home() {
        let devices: Vec<String> = state.fleet.devices().into_iter().map(|d| d.id).collect();
        let code_machine = state.fleet.code_device(&config.device_id);
        let own = state.registry.list();
        state
            .activity
            .update(&own, &config.device_id, &devices, &code_machine)
    } else {
        // Only the home has the tasks to change and every machine's sessions
        state.activity.clear()
    };
    let events = state.registry.sender();
    for (task, activity) in changed {
        let _ = events.send(Event::TaskActivity { task, activity });
    }
}

/// Events were missed: ask every machine for its sessions as they are now.
async fn resync(state: &AppState) {
    let asked = state.fleet.devices().into_iter().map(|device| {
        let client = state.fleet.client().clone();
        async move {
            let res = client
                .get(format!("{}/v0/sessions", device.address))
                .bearer_auth(&device.token)
                .timeout(Duration::from_secs(5))
                .send()
                .await
                .ok()?;
            let sessions: Vec<serde_json::Value> = res.json().await.ok()?;
            Some((device.id, sessions))
        }
    });
    for (device, sessions) in futures_util::future::join_all(asked)
        .await
        .into_iter()
        .flatten()
    {
        state.activity.replace(&device, sessions);
    }
}

/// Keep the board up to date. Start it before the relays, so no machine's
/// first list of sessions is missed.
pub fn spawn_board(state: AppState) {
    let mut events = state.registry.subscribe();
    tokio::spawn(async move {
        load_tasks(&state).await;
        refresh(&state);
        loop {
            match events.recv().await {
                Ok(event) => {
                    let tasks_changed = match &event {
                        Event::WorkspaceChanged { areas } => {
                            areas.is_empty() || areas.iter().any(|area| area == "tasks")
                        }
                        Event::HomeChanged { .. } => true,
                        _ => false,
                    };
                    if tasks_changed {
                        load_tasks(&state).await;
                    }
                    if tasks_changed || state.activity.see(&event) {
                        refresh(&state);
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    resync(&state).await;
                    load_tasks(&state).await;
                    refresh(&state);
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}
