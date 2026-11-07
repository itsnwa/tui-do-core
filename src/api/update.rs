//! Updating this core: `GET /v0/update` says which version runs here and the
//! latest release (as last checked; it never waits for GitHub), `POST /v0/update` installs a release and restarts the core
//! (see `update`). On the home core, devices are updated through
//! `POST /v0/fleet/devices/{id}/update`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};
use crate::update;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/v0/update", get(status).post(install))
}

async fn status() -> Response {
    Json(json!({
        "current": env!("CARGO_PKG_VERSION"),
        "latest": update::latest_known(),
        "canUpdate": update::target().is_some() && update::restarts_itself(),
    }))
    .into_response()
}

#[derive(Default, Deserialize)]
#[serde(default)]
pub(super) struct UpdateBody {
    /// Release to install; defaults to the latest.
    pub version: Option<String>,
}

async fn install(body: Option<Json<UpdateBody>>) -> Response {
    let version = match body.and_then(|Json(b)| b.version) {
        Some(version) => version,
        None => match update::latest().await {
            Ok(version) => version,
            Err(err) => return error(StatusCode::BAD_GATEWAY, &format!("{err:#}")),
        },
    };
    match update::install(&version).await {
        Ok(()) => {
            update::restart_soon();
            Json(json!({ "version": version })).into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}
