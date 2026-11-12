//! Dictation (see `crate::dictation`): a client sends what it recorded and
//! gets it back as text, transcribed and tidied with the home core's OpenAI
//! key, which a client sets but never reads.

use std::time::Instant;

use axum::extract::{DefaultBodyLimit, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::{AppState, error};
use crate::dictation::{self, Context, Keys, Writing};
use crate::workspace::tasks::TaskFilter;

/// Largest recording: minutes of speech as AAC, as base64 in JSON.
const MAX_RECORDING: usize = 16 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v0/dictation", get(status))
        .route("/v0/dictation/key", put(set_key))
        .route(
            "/v0/dictation/text",
            post(dictate).layer(DefaultBodyLimit::max(MAX_RECORDING)),
        )
}

fn describe(keys: &Keys) -> Response {
    Json(json!({ "available": keys.openai.is_some() })).into_response()
}

async fn status(State(state): State<AppState>) -> Response {
    describe(&dictation::load(&state.core_dir))
}

/// OpenAI's key to keep (checked first), or `""` to remove it.
#[derive(Deserialize)]
struct KeyChange {
    openai: String,
}

async fn set_key(State(state): State<AppState>, Json(change): Json<KeyChange>) -> Response {
    let given = change.openai.trim().to_string();
    if !given.is_empty() {
        // Keys are tokens; anything else in one is a mistake
        let valid = (8..=512).contains(&given.len())
            && given
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
        if !valid {
            return error(
                StatusCode::BAD_REQUEST,
                "that doesn't look like an OpenAI key",
            );
        }
        if let Err(err) = dictation::check(&given).await {
            return error(StatusCode::BAD_REQUEST, &format!("{err:#}"));
        }
    }
    let keys = Keys {
        openai: Some(given).filter(|k| !k.is_empty()),
    };
    match dictation::save(&state.core_dir, &keys) {
        Ok(()) => {
            tracing::info!(available = keys.openai.is_some(), "dictation key changed");
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
    /// What the client heard itself; used when there's no recording or
    /// transcribing it fails.
    #[serde(default)]
    heard: String,
    /// The language spoken, as a code (`en`).
    language: Option<String>,
    /// The session dictated into: its task gives the context.
    session: Option<String>,
    /// What the session's terminal shows.
    screen: Option<String>,
    /// What's being dictated: a message (the default), a new task, or
    /// more of a task's description.
    #[serde(default)]
    writing: Writing,
    /// For a task: the project it goes in, whose other open tasks give the
    /// context.
    project: Option<String>,
}

/// The session's task and project, or the new task's project and its open
/// tasks, from the workspace.
fn context(state: &AppState, recording: &Recording) -> Context {
    let mut context = Context {
        writing: recording.writing,
        screen: recording.screen.clone(),
        ..Context::default()
    };
    let Some(workspace) = state.workspace.current() else {
        return context;
    };
    if let Some(session) = &recording.session {
        if let Ok(detail) = workspace.get_task(session) {
            let project = workspace.get_project(&detail.task.project).ok();
            context.project = Some(project.map_or(detail.task.project.clone(), |p| p.title));
            context.task = Some(detail.task.title);
            context.description = Some(detail.description);
        }
    } else if let Some(slug) = recording.project.as_deref().filter(|p| !p.is_empty())
        && let Ok(project) = workspace.get_project(slug)
    {
        context.project = Some(project.title);
        let filter = TaskFilter {
            project: Some(slug.to_string()),
            ..TaskFilter::default()
        };
        context.open_tasks = workspace
            .list_tasks(&filter)
            .unwrap_or_default()
            .into_iter()
            .filter(|t| t.state != "completed")
            .take(dictation::OPEN_TASKS)
            .map(|t| t.title)
            .collect();
    }
    context
}

/// The recording as text: `{ transcript, transcribed, text }`. `transcript`
/// is what was said (OpenAI's, or else what the client heard); `text` is it
/// tidied (or as it is, when it was clean already), `null` without a key or
/// when tidying failed. A new task also has `title` and `description`
/// (`""` when the title says it all), and `text` is the two together.
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
    let mut task = None;
    let mut tidied = false;
    if let Some(key) = &keys.openai
        && !transcript.is_empty()
        && recording.writing == Writing::Task
    {
        let short = dictation::as_title(&transcript).filter(|_| transcribed);
        if let Some(title) = short {
            task = Some(dictation::Said {
                title,
                description: String::new(),
            });
        } else {
            match dictation::split(key, &transcript, &context).await {
                Ok(said) => {
                    task = Some(said);
                    tidied = true;
                }
                Err(err) => tracing::warn!("dictation: splitting the task failed: {err:#}"),
            }
        }
        text = task.as_ref().map(|t| {
            [t.title.as_str(), t.description.as_str()]
                .into_iter()
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        });
    } else if let Some(key) = &keys.openai
        && !transcript.is_empty()
    {
        // The client's own transcript always needs it; OpenAI's punctuates
        if !transcribed || dictation::needs_tidying(&transcript) {
            match dictation::tidy(key, &transcript, &context).await {
                Ok(done) => {
                    text = Some(done);
                    tidied = true;
                }
                Err(err) => tracing::warn!("dictation: tidying failed: {err:#}"),
            }
        } else {
            text = Some(transcript.clone());
        }
    }
    let total_ms = started.elapsed().as_millis();
    tracing::info!(
        writing = ?recording.writing,
        transcribed,
        tidied,
        transcribe_ms,
        tidy_ms = total_ms - transcribe_ms,
        total_ms,
        "dictation"
    );
    Json(json!({
        "transcript": transcript,
        "transcribed": transcribed,
        "text": text,
        "title": task.as_ref().map(|t| &t.title),
        "description": task.as_ref().map(|t| &t.description),
    }))
    .into_response()
}
