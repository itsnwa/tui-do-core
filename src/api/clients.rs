//! Paired clients (see `crate::clients`): listing, pairing and revoking.
//! Only the core's own token can use these routes.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/clients", get(list).post(create))
        .route("/v0/clients/{id}", delete(revoke))
}

/// Routes a paired client may not use, also through `/v0/devices/{id}/…`.
pub fn owner_only(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let rest = match path.strip_prefix("/v0/devices/") {
        Some(rest) => rest.split_once('/').map_or("", |(_, rest)| rest),
        None => path.strip_prefix("/v0/").unwrap_or(""),
    };
    rest == "clients" || rest.starts_with("clients/")
}

async fn list(State(state): State<AppState>) -> Response {
    Json(state.clients.list()).into_response()
}

#[derive(Deserialize)]
struct CreateClient {
    name: String,
}

/// Pair a client: answers with the client and its token (shown only now).
async fn create(State(state): State<AppState>, Json(body): Json<CreateClient>) -> Response {
    match state.clients.create(&body.name) {
        Ok((client, token)) => {
            tracing::info!(id = %client.id, name = %client.name, "client paired");
            Json(json!({ "client": client, "token": token })).into_response()
        }
        Err(err) => error(StatusCode::BAD_REQUEST, &format!("{err:#}")),
    }
}

async fn revoke(State(state): State<AppState>, Path(id): Path<String>) -> Response {
    match state.clients.revoke(&id) {
        Ok(true) => {
            tracing::info!(%id, "client revoked");
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => error(StatusCode::NOT_FOUND, "no such client"),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::owner_only;

    #[test]
    fn client_routes_should_be_owner_only_directly_and_through_devices() {
        assert!(owner_only("/v0/clients"));
        assert!(owner_only("/v0/clients/abc"));
        assert!(owner_only("/v0/devices/xyz/clients"));
        assert!(owner_only("/v0/devices/xyz/clients/abc/"));
        assert!(!owner_only("/v0/tasks"));
        assert!(!owner_only("/v0/devices/xyz/sessions"));
        assert!(!owner_only("/v0/clientsx"));
    }
}
