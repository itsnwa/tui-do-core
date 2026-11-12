//! How much of their plans the agents have used (see `crate::usage`):
//! `GET /v0/usage/here` on this machine, and `GET /v0/usage` across the
//! fleet, the newest answer for each agent from whichever machine has one.

use std::time::Duration;

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use axum::routing::get;

use super::AppState;
use crate::usage::{self, Report, freshest};
use crate::workspace::dates::now_millis;

/// How long another machine gets to answer: it may be asking its services.
const MACHINE_TIMEOUT: Duration = Duration::from_secs(15);

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/usage", get(everywhere))
        .route("/v0/usage/here", get(here))
}

/// This machine's, each answer saying it's from here.
async fn local(state: &AppState) -> Report {
    let name = state.config.get().device_name;
    let report = usage::here(&state.core_dir).await;
    Report {
        claude: report.claude.from_machine(&name),
        codex: report.codex.from_machine(&name),
    }
}

async fn here(State(state): State<AppState>) -> Response {
    Json(local(&state).await).into_response()
}

async fn everywhere(State(state): State<AppState>) -> Response {
    let own = state.config.get().device_id;
    let others = state
        .fleet
        .devices()
        .into_iter()
        .filter(|device| device.id != own)
        .map(|device| {
            let request = state
                .fleet
                .client()
                .get(format!("{}/v0/usage/here", device.address))
                .bearer_auth(&device.token)
                .timeout(MACHINE_TIMEOUT)
                .send();
            async move {
                // A machine whose core is too old to say, or that's away, says nothing
                let res = request.await.ok()?.error_for_status().ok()?;
                let report: Report = res.json().await.ok()?;
                Some(Report {
                    claude: report.claude.from_machine(&device.name),
                    codex: report.codex.from_machine(&device.name),
                })
            }
        });
    let (mine, others) = tokio::join!(local(&state), futures_util::future::join_all(others));
    let report = others
        .into_iter()
        .flatten()
        .fold(mine, |best, other| Report {
            claude: freshest(best.claude, other.claude),
            codex: freshest(best.codex, other.codex),
        });
    let now = now_millis();
    Json(Report {
        claude: report.claude.aged(now),
        codex: report.codex.aged(now),
    })
    .into_response()
}
