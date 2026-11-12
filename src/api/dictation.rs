//! Dictation (see `crate::dictation`): a client sends what it recorded and
//! gets it back as text, transcribed and tidied with the home core's keys,
//! which a client sets but never reads.

use std::time::Instant;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};
use crate::dictation::{self, Context, Keys, Service};

/// Largest recording: minutes of speech as AAC, as base64 in JSON.
const MAX_RECORDING: usize = 16 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v0/dictation", get(status))
        .route("/v0/dictation/keys", put(set_keys))
        .route(
            "/v0/dictation/text",
            post(dictate).layer(DefaultBodyLimit::max(MAX_RECORDING)),
        )
}

fn describe(keys: &Keys) -> Response {
    Json(json!({
        "transcribe": keys.openai.is_some(),
        "tidy": keys.anthropic.is_some(),
    }))
    .into_response()
}

async fn status(State(state): State<AppState>) -> Response {
    describe(&dictation::load(&state.core_dir))
}

/// A key to keep (checked first), or `""` to remove it; absent leaves it.
#[derive(Deserialize)]
struct KeysChange {
    openai: Option<String>,
    anthropic: Option<String>,
}

async fn set_keys(State(state): State<AppState>, Json(change): Json<KeysChange>) -> Response {
    let mut keys = dictation::load(&state.core_dir);
    for (service, given, kept) in [
        (Service::OpenAi, change.openai, &mut keys.openai),
        (Service::Anthropic, change.anthropic, &mut keys.anthropic),
    ] {
        let Some(given) = given.map(|k| k.trim().to_string()) else {
            continue;
        };
        if given.is_empty() {
            *kept = None;
            continue;
        }
        if let Err(err) = dictation::check(service, &given).await {
            return error(StatusCode::BAD_REQUEST, &format!("{err:#}"));
        }
        *kept = Some(given);
    }
    match dictation::save(&state.core_dir, &keys) {
        Ok(()) => {
            tracing::info!(
                transcribe = keys.openai.is_some(),
                tidy = keys.anthropic.is_some(),
                "dictation keys changed"
            );
            describe(&keys)
        }
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &format!("{err:#}")),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recording {
    /// What was recorded, as an m4a (AAC), base64.
    audio_base64: Option<String>,
    /// What the client heard itself; used when there's no recording or no
    /// OpenAI key, or OpenAI fails.
    #[serde(default)]
    heard: String,
    /// The language spoken, as a code (`en`).
    language: Option<String>,
    /// The session dictated into: its task gives the context.
    session: Option<String>,
    /// What the session's terminal shows.
    screen: Option<String>,
}

/// The session's task and project, from the workspace.
fn context(state: &AppState, recording: &Recording) -> Context {
    let mut context = Context {
        screen: recording.screen.clone(),
        ..Context::default()
    };
    let (Some(workspace), Some(session)) = (state.workspace.current(), &recording.session) else {
        return context;
    };
    if let Ok(detail) = workspace.get_task(session) {
        let project = workspace.get_project(&detail.task.project).ok();
        context.project = Some(project.map_or(detail.task.project.clone(), |p| p.title));
        context.task = Some(detail.task.title);
        context.description = Some(detail.description);
    }
    context
}

/// The recording as text: `{ transcript, text }`. `transcript` is what was
/// said (OpenAI's, or else what the client heard); `text` is it tidied by
/// Claude, `null` without an Anthropic key or when Claude failed.
async fn dictate(State(state): State<AppState>, Json(recording): Json<Recording>) -> Response {
    use base64::Engine;
    let keys = dictation::load(&state.core_dir);
    let context = context(&state, &recording);
    let started = Instant::now();

    let mut transcript = recording.heard.trim().to_string();
    let mut transcribed = false;
    if let (Some(key), Some(audio)) = (&keys.openai, &recording.audio_base64) {
        let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(audio) else {
            return error(StatusCode::BAD_REQUEST, "audioBase64 isn't valid base64");
        };
        let dir = state.core_dir.join("dictation");
        let file = dir.join(format!("{}.m4a", uuid::Uuid::new_v4()));
        let written = async {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(&file, &bytes).await
        }
        .await;
        let heard = match written {
            Ok(()) => {
                dictation::transcribe(key, &file, recording.language.as_deref(), &context).await
            }
            Err(err) => Err(err.into()),
        };
        let _ = tokio::fs::remove_file(&file).await;
        match heard {
            Ok(heard) => {
                transcript = heard;
                transcribed = true;
            }
            Err(err) => tracing::warn!("dictation: transcribing failed: {err:#}"),
        }
    }
    let transcribe_ms = started.elapsed().as_millis();

    let mut text = None;
    if let Some(key) = &keys.anthropic
        && !transcript.is_empty()
    {
        match dictation::tidy(key, &transcript, &context).await {
            Ok(tidied) => text = Some(tidied),
            Err(err) => tracing::warn!("dictation: tidying failed: {err:#}"),
        }
    }
    tracing::info!(
        transcribed,
        tidied = text.is_some(),
        transcribe_ms,
        total_ms = started.elapsed().as_millis(),
        "dictation"
    );
    Json(json!({
        "transcript": transcript,
        "transcribed": transcribed,
        "text": text,
    }))
    .into_response()
}
