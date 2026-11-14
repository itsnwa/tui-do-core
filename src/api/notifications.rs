//! Phone notifications (see `crate::notify`): a paired client turns its own
//! on or off, and the home core sends them through APNs when an agent waits.
//! Also the pushes that follow agents (see `crate::live`): a client's widgets
//! load again, and its Live Activities start, change and end.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;

use super::{AppState, error};
use crate::apns::{self, Alert, Kind, Sent, Target};
use crate::clients::{Caller, LiveTokens};
use crate::live::{self, Agent, Followed, Push};
use crate::notify::{self, SessionKey, Waits};
use crate::session::Event;

/// How long a burst of changes settles before the widgets are told.
const WIDGETS_SETTLE: Duration = Duration::from_secs(2);
/// How often a client's widgets are told to load again, at most: APNs
/// budgets these pushes.
const WIDGETS_EVERY: Duration = Duration::from_secs(30);

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v0/notifications", get(status).put(set))
        .route("/v0/notifications/test", post(test))
        .route(
            "/v0/notifications/widgets",
            put(set_widgets).delete(clear_widgets),
        )
        .route("/v0/notifications/live-activities", put(set_live))
        .route(
            "/v0/notifications/live-activities/{device}/{session}",
            put(set_activity).delete(clear_activity),
        )
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
        "widgets": state.clients.has_widgets(id),
        "liveActivities": state.clients.live(id).is_some(),
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

/// A push token and where it's from: the APNs server of the app's build, and
/// its bundle id.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Token {
    token: String,
    environment: String,
    topic: String,
}

impl Token {
    fn target(self) -> anyhow::Result<Target> {
        let target = Target {
            device_token: self.token.to_ascii_lowercase(),
            environment: self.environment,
            topic: self.topic,
        };
        target.validate()?;
        Ok(target)
    }
}

fn saved(state: &AppState, id: &str, result: anyhow::Result<()>) -> Response {
    match result {
        Ok(()) => describe(state, id),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

/// Have the client's widgets load again when an agent's state changes.
async fn set_widgets(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Json(token): Json<Token>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let target = match token.target() {
        Ok(target) => target,
        Err(err) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    };
    saved(&state, id, state.clients.set_widgets(id, Some(target)))
}

async fn clear_widgets(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    saved(&state, id, state.clients.set_widgets(id, None))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct LiveChange {
    enabled: bool,
    start_token: Option<String>,
    environment: Option<String>,
    topic: Option<String>,
}

/// Turn a client's Live Activities on, with the token that starts them, or
/// off. Turned on, the agents at work now get one too; the app sends its
/// token again at each launch, which changes nothing.
async fn set_live(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Json(change): Json<LiveChange>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let start = if change.enabled {
        let (Some(token), Some(environment), Some(topic)) =
            (change.start_token, change.environment, change.topic)
        else {
            return error(
                StatusCode::BAD_REQUEST,
                "turning Live Activities on needs startToken, environment and topic",
            );
        };
        match (Token {
            token,
            environment,
            topic,
        })
        .target()
        {
            Ok(target) => Some(target),
            Err(err) => return error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
        }
    } else {
        None
    };
    let kept = state.clients.live(id).map(|live| live.start);
    if kept == start {
        return describe(&state, id);
    }
    if let Err(err) = state.clients.set_live(id, start) {
        return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
    }
    tracing::info!(client = %id, on = change.enabled, "live activities changed");
    if change.enabled {
        tokio::spawn(start_running(state.clone(), id.to_string()));
    }
    describe(&state, id)
}

#[derive(Deserialize)]
struct ActivityToken {
    token: String,
}

/// Keep the token of the client's Live Activity for a session, which it
/// gets once the activity started. What the agent shows now goes to it right
/// away: it may have moved on, or stopped, since.
async fn set_activity(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Path((device, session)): Path<(String, String)>,
    Json(body): Json<ActivityToken>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    let Some(live) = state.clients.live(id) else {
        return error(StatusCode::CONFLICT, "Live Activities are off");
    };
    let token = body.token.to_ascii_lowercase();
    let target = live.target(&token);
    if let Err(err) = target.validate() {
        return error(StatusCode::BAD_REQUEST, &format!("{err:#}"));
    }
    let key = live::key(&device, &session);
    let activity = state.activity.all().remove(&session);
    let now = live::agent(&session, activity.as_ref()).filter(|agent| agent.device == device);
    let payload = match &now {
        Some(agent) => {
            if let Err(err) = state.clients.set_activity_token(id, &key, Some(token)) {
                return error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}"));
            }
            live::update_payload(&agent.shown, unix_now())
        }
        None => {
            let shown = live::Shown {
                state: activity.map_or("done", |a| a.state),
                detail: None,
            };
            live::end_payload(&shown, unix_now())
        }
    };
    tokio::spawn(async move {
        if let Some(key) = apns::load(&state.core_dir) {
            send_live(&state, &key, &target, &payload, 10).await;
        }
    });
    StatusCode::NO_CONTENT.into_response()
}

async fn clear_activity(
    State(state): State<AppState>,
    caller: Option<Extension<Caller>>,
    Path((device, session)): Path<(String, String)>,
) -> Response {
    let Some(id) = client(caller.as_deref()) else {
        return not_a_client();
    };
    match state
        .clients
        .set_activity_token(id, &live::key(&device, &session), None)
    {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
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

/// Send a push of any kind, forgetting a token APNs says is gone.
async fn forward(
    state: &AppState,
    key: &apns::Key,
    target: &Target,
    kind: Kind,
    payload: &serde_json::Value,
    priority: Option<u8>,
) -> anyhow::Result<()> {
    match apns::push(key, target, kind, payload, priority).await? {
        Sent::Delivered => Ok(()),
        Sent::Unregistered => {
            state.clients.forget_device_token(&target.device_token)?;
            anyhow::bail!("the token isn't registered any more")
        }
    }
}

/// Follow agents with pushes: tell clients' widgets to load again (after a
/// burst of changes settles, at most every `WIDGETS_EVERY`), and start,
/// update and end their Live Activities.
pub fn spawn_pusher(state: AppState) {
    let mut events = state.registry.subscribe();
    let (pushes, mut queue) = tokio::sync::mpsc::unbounded_channel::<Push>();
    // One at a time, so a Live Activity's pushes arrive in order
    let sender = state.clone();
    tokio::spawn(async move {
        while let Some(push) = queue.recv().await {
            follow(&sender, push).await;
        }
    });
    tokio::spawn(async move {
        let mut followed = Followed::default();
        let mut widgets_due: Option<Instant> = None;
        let mut widgets_sent: Option<Instant> = None;
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(Event::TaskActivity { task, activity }) => {
                        for push in followed.change(&task, activity.as_ref()) {
                            if push == Push::Widgets {
                                let now = Instant::now();
                                let earliest = widgets_sent.map_or(now, |at| at + WIDGETS_EVERY);
                                widgets_due.get_or_insert((now + WIDGETS_SETTLE).max(earliest));
                            } else {
                                let _ = pushes.send(push);
                            }
                        }
                    }
                    Ok(_) => {}
                    // The board catches up and says what changed
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return,
                },
                () = tokio::time::sleep_until(widgets_due.unwrap_or_else(Instant::now)), if widgets_due.is_some() => {
                    widgets_due = None;
                    widgets_sent = Some(Instant::now());
                    tokio::spawn(push_widgets(state.clone()));
                }
            }
        }
    });
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Have every client's widgets load again.
async fn push_widgets(state: AppState) {
    // Only the home has the clients and every machine's agents
    if !state.config.get().is_home() {
        return;
    }
    let targets = state.clients.widget_targets();
    if targets.is_empty() {
        return;
    }
    let Some(key) = apns::load(&state.core_dir) else {
        return;
    };
    let payload = apns::widgets_payload();
    for target in targets {
        match forward(&state, &key, &target, Kind::Widgets, &payload, None).await {
            Ok(()) => tracing::info!("told a client's widgets to load"),
            Err(err) => {
                tracing::warn!(error = %format!("{err:#}"), "couldn't tell widgets to load")
            }
        }
    }
}

/// Send a Live Activity push to each client with Live Activities on.
async fn follow(state: &AppState, push: Push) {
    if !state.config.get().is_home() {
        return;
    }
    let clients = state.clients.live_clients();
    if clients.is_empty() {
        return;
    }
    let Some(key) = apns::load(&state.core_dir) else {
        return;
    };
    let now = unix_now();
    match push {
        Push::Widgets => {}
        Push::Start(agent) => {
            let about = about(state, &agent).await;
            for (_, live) in clients {
                start(state, &key, &live, &agent, &about, now).await;
            }
        }
        Push::Update(agent) => {
            let payload = live::update_payload(&agent.shown, now);
            for (_, live) in clients {
                if let Some(token) = live.activities.get(&agent.key()) {
                    send_live(
                        state,
                        &key,
                        &live.target(token),
                        &payload,
                        agent.shown.priority(),
                    )
                    .await;
                }
            }
        }
        Push::End {
            key: session,
            shown,
        } => {
            let payload = live::end_payload(&shown, now);
            for (client, live) in clients {
                if let Some(token) = live.activities.get(&session) {
                    send_live(state, &key, &live.target(token), &payload, 10).await;
                    let _ = state.clients.set_activity_token(&client, &session, None);
                }
            }
        }
    }
}

/// Start an agent's Live Activity on a client, or bring the one it has (from
/// before the core restarted, say) up to date.
async fn start(
    state: &AppState,
    key: &apns::Key,
    live: &LiveTokens,
    agent: &Agent,
    about: &live::About,
    now: u64,
) {
    match live.activities.get(&agent.key()) {
        Some(token) => {
            let payload = live::update_payload(&agent.shown, now);
            send_live(state, key, &live.target(token), &payload, 10).await;
        }
        None => {
            let payload = live::start_payload(agent, about, now);
            send_live(state, key, &live.start, &payload, 10).await;
        }
    }
}

/// Start Live Activities for the agents at work now, on a client that just
/// turned them on.
async fn start_running(state: AppState, client: String) {
    if !state.config.get().is_home() {
        return;
    }
    let (Some(live), Some(key)) = (state.clients.live(&client), apns::load(&state.core_dir)) else {
        return;
    };
    for (task, activity) in state.activity.all() {
        if let Some(agent) = live::agent(&task, Some(&activity)) {
            let about = about(&state, &agent).await;
            start(&state, &key, &live, &agent, &about, unix_now()).await;
        }
    }
}

async fn send_live(
    state: &AppState,
    key: &apns::Key,
    target: &Target,
    payload: &serde_json::Value,
    priority: u8,
) {
    let event = payload["aps"]["event"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    match forward(
        state,
        key,
        target,
        Kind::LiveActivity,
        payload,
        Some(priority),
    )
    .await
    {
        Ok(()) => tracing::info!(%event, "live activity push sent"),
        Err(err) => {
            tracing::warn!(%event, error = %format!("{err:#}"), "couldn't send a live activity push")
        }
    }
}

/// What a Live Activity says of an agent's task and machine.
async fn about(state: &AppState, agent: &Agent) -> live::About {
    let workspace = state.workspace.current();
    let slug = agent.task.clone();
    let task = tokio::task::spawn_blocking(move || {
        workspace.and_then(|ws| ws.get_task(&slug).ok().map(|t| (t.task.id, t.task.title)))
    })
    .await
    .ok()
    .flatten();
    let (id, title) = task.unwrap_or_else(|| (String::new(), agent.task.clone()));
    live::About {
        id,
        title,
        machine: machine_name(state, &agent.device),
    }
}

/// A machine's name, by its device id.
fn machine_name(state: &AppState, device: &str) -> String {
    let config = state.config.get();
    if device == config.device_id {
        config.device_name
    } else {
        state
            .fleet
            .device(device)
            .map_or_else(|| device.to_string(), |d| d.name)
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
    let machine = machine_name(&state, device);
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
