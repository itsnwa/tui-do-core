//! Workspace routes: tasks, projects, plans, daily notes and weekly updates.
//! Workspace operations do file I/O, so each runs on a blocking thread.

use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{AppState, error};
use crate::workspace::plans::SavePlan;
use crate::workspace::projects::{CreateProject, UpdateProject};
use crate::workspace::tasks::{CreateTask, DeletedTask, TaskFilter, UpdateTask};
use crate::workspace::{Error, Workspace};

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/workspace", get(get_workspace).put(set_workspace))
        .route("/v0/tasks", get(list_tasks).post(create_task))
        .route("/v0/tasks/lookup", get(lookup_task))
        .route("/v0/tasks/restore", post(restore_task))
        .route(
            "/v0/tasks/{slug}",
            get(get_task).patch(update_task).delete(delete_task),
        )
        .route("/v0/tasks/{slug}/reorder", post(reorder_task))
        .route("/v0/tasks/{slug}/attachments", post(add_attachment))
        .route(
            "/v0/tasks/{slug}/attachments/remove",
            post(remove_attachment),
        )
        .route("/v0/tasks/{slug}/attachments/{index}", get(attachment_file))
        .route("/v0/tasks/{reference}/start", post(start_task))
        .route("/v0/tasks/{reference}/complete", post(complete_task))
        .route("/v0/tasks/{reference}/plan", put(write_task_plan))
        .route("/v0/projects", get(list_projects).post(create_project))
        .route(
            "/v0/projects/{slug}",
            get(get_project)
                .patch(update_project)
                .delete(delete_project),
        )
        .route("/v0/plans", get(list_plans))
        .route(
            "/v0/plans/{slug}",
            get(get_plan).put(save_plan).delete(delete_plan),
        )
        .route("/v0/daily", get(list_daily))
        .route("/v0/daily/{date}", get(get_daily).put(save_daily))
        .route("/v0/daily/{date}/summary", put(set_daily_summary))
        .route("/v0/weekly", get(list_weekly))
        .route("/v0/weekly/{week}", get(get_weekly).put(save_weekly))
        .route("/v0/weekly/{week}/summary", put(set_weekly_summary))
}

/// Run a workspace operation on a blocking thread and turn its result into a response.
async fn run<T, F>(state: &AppState, op: F) -> Response
where
    T: Serialize + Send + 'static,
    F: FnOnce(&Workspace) -> crate::workspace::Result<T> + Send + 'static,
{
    let Some(ws) = state.workspace.current() else {
        return error(
            StatusCode::CONFLICT,
            "Workspace not configured. Please select a workspace folder first.",
        );
    };
    match tokio::task::spawn_blocking(move || op(&ws)).await {
        Ok(Ok(value)) => Json(value).into_response(),
        Ok(Err(Error::NotFound(msg))) => error(StatusCode::NOT_FOUND, &msg),
        Ok(Err(Error::Invalid(msg))) => error(StatusCode::BAD_REQUEST, &msg),
        Ok(Err(err)) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

// --- workspace ---

async fn get_workspace(State(state): State<AppState>) -> Response {
    let path = state
        .workspace
        .current()
        .map(|ws| ws.root().display().to_string());
    Json(json!({ "workspacePath": path })).into_response()
}

#[derive(Deserialize)]
struct SetWorkspace {
    path: PathBuf,
}

async fn set_workspace(State(state): State<AppState>, Json(body): Json<SetWorkspace>) -> Response {
    let shared = Arc::clone(&state.workspace);
    let path = body.path.clone();
    match tokio::task::spawn_blocking(move || shared.set(&path)).await {
        Ok(Ok(())) => Json(json!({ "workspacePath": body.path })).into_response(),
        Ok(Err(Error::Invalid(msg))) => error(StatusCode::BAD_REQUEST, &msg),
        Ok(Err(err)) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

// --- tasks ---

async fn list_tasks(State(state): State<AppState>, Query(filter): Query<TaskFilter>) -> Response {
    run(&state, move |ws| ws.list_tasks(&filter)).await
}

async fn create_task(State(state): State<AppState>, Json(req): Json<CreateTask>) -> Response {
    run(&state, move |ws| ws.create_task(req)).await
}

#[derive(Deserialize)]
struct LookupQuery {
    #[serde(rename = "ref")]
    reference: String,
}

async fn lookup_task(State(state): State<AppState>, Query(q): Query<LookupQuery>) -> Response {
    run(&state, move |ws| match ws.find_task(&q.reference)? {
        Some(slug) => ws.get_task(&slug),
        None => Err(Error::NotFound(format!(
            "Task not found: \"{}\"",
            q.reference
        ))),
    })
    .await
}

async fn get_task(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| ws.get_task(&slug)).await
}

async fn update_task(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<UpdateTask>,
) -> Response {
    let completing = req.state.as_deref() == Some("completed");
    let task = slug.clone();
    let response = run(&state, move |ws| ws.update_task(&slug, req)).await;
    // A task that ran on another machine brings its work back when it's done
    if completing && response.status().is_success() && state.handoffs.record(&task).is_some() {
        let handoffs = std::sync::Arc::clone(&state.handoffs);
        tokio::spawn(async move { handoffs.bring_back(&task).await });
    }
    response
}

async fn delete_task(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| ws.delete_task(&slug)).await
}

async fn restore_task(State(state): State<AppState>, Json(req): Json<DeletedTask>) -> Response {
    run(&state, move |ws| ws.restore_task(req)).await
}

#[derive(Deserialize)]
struct ReorderBody {
    direction: String,
    project: Option<String>,
}

async fn reorder_task(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<ReorderBody>,
) -> Response {
    let up = match body.direction.as_str() {
        "up" => true,
        "down" => false,
        _ => return error(StatusCode::BAD_REQUEST, "direction must be 'up' or 'down'"),
    };
    run(&state, move |ws| {
        ws.reorder_task(&slug, up, body.project.as_deref())
    })
    .await
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AttachmentBody {
    file_path: Option<String>,
    file_name: Option<String>,
    data_base64: Option<String>,
}

async fn add_attachment(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<AttachmentBody>,
) -> Response {
    run(&state, move |ws| {
        let path = match (body.file_path, body.file_name, body.data_base64) {
            (_, Some(name), Some(data)) => ws.save_attachment_file(&slug, &name, &data)?,
            (Some(path), _, _) => path,
            _ => {
                return Err(Error::Invalid(
                    "send either filePath or fileName + dataBase64".into(),
                ));
            }
        };
        ws.add_attachment(&slug, &path)
    })
    .await
}

async fn remove_attachment(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(body): Json<AttachmentBody>,
) -> Response {
    let Some(path) = body.file_path else {
        return error(StatusCode::BAD_REQUEST, "filePath is required");
    };
    run(&state, move |ws| ws.remove_attachment(&slug, &path)).await
}

/// The type a client needs to show an attachment, from its extension.
fn content_type(path: &std::path::Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase);
    match ext.as_deref() {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("heic") => "image/heic",
        Some("svg") => "image/svg+xml",
        Some("pdf") => "application/pdf",
        Some("json") => "application/json",
        Some("md" | "markdown") => "text/markdown; charset=utf-8",
        Some("txt" | "log" | "csv") => "text/plain; charset=utf-8",
        Some("mov") => "video/quicktime",
        Some("mp4") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// One of a task's attachments, by its position in the task's list. Only
/// files the task lists are served, wherever they are.
async fn attachment_file(
    State(state): State<AppState>,
    Path((slug, index)): Path<(String, usize)>,
) -> Response {
    let Some(ws) = state.workspace.current() else {
        return error(StatusCode::CONFLICT, "Workspace not configured.");
    };
    let root = ws.root().to_path_buf();
    let listed = tokio::task::spawn_blocking(move || ws.get_task(&slug)).await;
    let path = match listed {
        Ok(Ok(task)) => match task.task.attachments.get(index) {
            Some(path) => {
                crate::session::expand_home(&crate::workspace::local_attachment(path, &root))
            }
            None => return error(StatusCode::NOT_FOUND, "no such attachment"),
        },
        Ok(Err(Error::NotFound(msg))) => return error(StatusCode::NOT_FOUND, &msg),
        Ok(Err(err)) => return error(StatusCode::BAD_REQUEST, &err.to_string()),
        Err(err) => return error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    };
    match tokio::fs::read(&path).await {
        Ok(bytes) => ([(header::CONTENT_TYPE, content_type(&path))], bytes).into_response(),
        Err(err) => error(
            StatusCode::NOT_FOUND,
            &format!("couldn't read {}: {err}", path.display()),
        ),
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct StartBody {
    expected_project: Option<String>,
}

async fn start_task(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    body: Option<Json<StartBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    run(&state, move |ws| {
        ws.start_task(&reference, body.expected_project.as_deref())
    })
    .await
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct CompleteBody {
    summary: Option<String>,
}

async fn complete_task(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    body: Option<Json<CompleteBody>>,
) -> Response {
    let body = body.map(|Json(b)| b).unwrap_or_default();
    run(&state, move |ws| {
        ws.complete_task(&reference, body.summary.as_deref())
    })
    .await
}

#[derive(Deserialize)]
struct ContentBody {
    content: String,
}

async fn write_task_plan(
    State(state): State<AppState>,
    Path(reference): Path<String>,
    Json(body): Json<ContentBody>,
) -> Response {
    run(&state, move |ws| {
        ws.write_task_plan(&reference, &body.content)
    })
    .await
}

// --- projects ---

async fn list_projects(State(state): State<AppState>) -> Response {
    run(&state, |ws| ws.list_projects()).await
}

async fn create_project(State(state): State<AppState>, Json(req): Json<CreateProject>) -> Response {
    run(&state, move |ws| ws.create_project(req)).await
}

async fn get_project(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| ws.get_project(&slug)).await
}

async fn update_project(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<UpdateProject>,
) -> Response {
    run(&state, move |ws| ws.update_project(&slug, req)).await
}

async fn delete_project(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| {
        ws.delete_project(&slug)
            .map(|()| json!({ "deleted": slug }))
    })
    .await
}

// --- plans ---

async fn list_plans(State(state): State<AppState>) -> Response {
    run(&state, |ws| ws.list_plans()).await
}

async fn get_plan(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| ws.get_plan(&slug)).await
}

async fn save_plan(
    State(state): State<AppState>,
    Path(slug): Path<String>,
    Json(req): Json<SavePlan>,
) -> Response {
    run(&state, move |ws| ws.save_plan(&slug, req)).await
}

async fn delete_plan(State(state): State<AppState>, Path(slug): Path<String>) -> Response {
    run(&state, move |ws| {
        ws.delete_plan(&slug).map(|()| json!({ "deleted": slug }))
    })
    .await
}

// --- daily notes ---

async fn list_daily(State(state): State<AppState>) -> Response {
    run(&state, |ws| ws.list_daily_notes()).await
}

async fn get_daily(State(state): State<AppState>, Path(date): Path<String>) -> Response {
    run(&state, move |ws| ws.get_daily_note(&date)).await
}

async fn save_daily(
    State(state): State<AppState>,
    Path(date): Path<String>,
    Json(body): Json<ContentBody>,
) -> Response {
    run(&state, move |ws| {
        ws.save_daily_note(&date, &body.content)
            .map(|()| json!({ "date": date }))
    })
    .await
}

#[derive(Deserialize)]
struct SummaryBody {
    content: String,
    summary: String,
}

async fn set_daily_summary(
    State(state): State<AppState>,
    Path(date): Path<String>,
    Json(body): Json<SummaryBody>,
) -> Response {
    run(&state, move |ws| {
        ws.set_daily_summary(&date, &body.content, &body.summary)
            .map(|saved| json!({ "saved": saved }))
    })
    .await
}

// --- weekly updates ---

async fn list_weekly(State(state): State<AppState>) -> Response {
    run(&state, |ws| ws.list_weekly_updates()).await
}

async fn get_weekly(State(state): State<AppState>, Path(week): Path<String>) -> Response {
    run(&state, move |ws| ws.get_weekly_update(&week)).await
}

#[derive(Deserialize)]
struct SaveWeeklyBody {
    content: String,
    /// The text was just generated: stamp `generated` and drop the old summary.
    #[serde(default)]
    generated: bool,
}

async fn save_weekly(
    State(state): State<AppState>,
    Path(week): Path<String>,
    Json(body): Json<SaveWeeklyBody>,
) -> Response {
    run(&state, move |ws| {
        ws.save_weekly_update(&week, &body.content, body.generated)
            .map(|generated| json!({ "weekId": week, "generated": generated }))
    })
    .await
}

async fn set_weekly_summary(
    State(state): State<AppState>,
    Path(week): Path<String>,
    Json(body): Json<SummaryBody>,
) -> Response {
    run(&state, move |ws| {
        ws.set_weekly_summary(&week, &body.content, &body.summary)
            .map(|saved| json!({ "saved": saved }))
    })
    .await
}
