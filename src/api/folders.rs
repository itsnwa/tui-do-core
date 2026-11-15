//! `GET /v0/fs/dirs`: the folders in a folder on this machine, for picking a
//! project's folder from another machine (through `/v0/devices/{id}/fs/dirs`).

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use serde::{Deserialize, Serialize};

use super::{AppState, error};
use crate::session::expand_home;

/// More than anyone scrolls through; the rest are left out.
const MAX_ENTRIES: usize = 2000;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new().route("/v0/fs/dirs", get(dirs))
}

#[derive(Deserialize)]
struct DirsQuery {
    #[serde(default)]
    path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Listing {
    /// The folder, as `~/…` when it's in the home folder.
    path: String,
    /// The folder it's in; none at `/`.
    parent: Option<String>,
    entries: Vec<Entry>,
    /// Some folders were left out (more than `MAX_ENTRIES`).
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct Entry {
    name: String,
    path: String,
    /// It's a git repo (has a `.git`).
    git: bool,
}

async fn dirs(Query(query): Query<DirsQuery>) -> Response {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let requested = if query.path.trim().is_empty() {
        "~".to_string()
    } else {
        query.path.trim().to_string()
    };
    match tokio::task::spawn_blocking(move || list(&requested, home.as_deref())).await {
        Ok(Ok(listing)) => Json(listing).into_response(),
        Ok(Err((status, message))) => error(status, &message),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// The folders in `path` (`~` and `~/…` are the home folder), hidden ones
/// left out, sorted by name.
fn list(path: &str, home: Option<&Path>) -> Result<Listing, (StatusCode, String)> {
    let dir = expand_home(path);
    if !dir.is_absolute() {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("{path} isn't an absolute path"),
        ));
    }
    let read = std::fs::read_dir(&dir).map_err(|err| {
        let status = match err.kind() {
            std::io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            _ => StatusCode::BAD_REQUEST,
        };
        (status, format!("can't open {path}: {err}"))
    })?;
    let mut entries: Vec<Entry> = read
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let full = entry.path();
            // Symlinks to folders count as folders
            (!name.starts_with('.') && full.is_dir()).then(|| Entry {
                git: full.join(".git").exists(),
                path: shown(&full, home),
                name,
            })
        })
        .collect();
    entries.sort_by_cached_key(|e| e.name.to_lowercase());
    let truncated = entries.len() > MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok(Listing {
        path: shown(&dir, home),
        parent: dir.parent().map(|p| shown(p, home)),
        entries,
        truncated,
    })
}

/// `path` as `~/…` when it's in the home folder, so it reads the same on any
/// machine; otherwise as it is.
fn shown(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> PathBuf {
        let root = std::env::temp_dir().join(format!("tui-do-folders-{}", uuid::Uuid::new_v4()));
        for dir in ["b-repo/.git", "A", ".hidden", "c"] {
            std::fs::create_dir_all(root.join(dir)).expect("dir");
        }
        std::fs::write(root.join("file.txt"), "x").expect("file");
        root
    }

    #[test]
    fn should_list_visible_folders_by_name_and_mark_repos() {
        let root = folder();
        let listing = list(&root.display().to_string(), None).expect("list");
        let names: Vec<_> = listing.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["A", "b-repo", "c"]);
        assert!(listing.entries[1].git && !listing.entries[0].git);
        assert_eq!(
            listing.parent.as_deref(),
            root.parent().and_then(Path::to_str)
        );
        assert!(!listing.truncated);
        std::fs::remove_dir_all(root).expect("clean up");
    }

    #[test]
    fn should_show_paths_in_the_home_folder_with_a_tilde() {
        let root = folder();
        let listing = list(&root.join("A").display().to_string(), Some(&root)).expect("list");
        assert_eq!(listing.path, "~/A");
        assert_eq!(listing.parent.as_deref(), Some("~"));
        let listing = list(&root.display().to_string(), Some(&root)).expect("list");
        assert_eq!(listing.entries[1].path, "~/b-repo");
        std::fs::remove_dir_all(root).expect("clean up");
    }

    #[test]
    fn should_refuse_relative_and_missing_folders() {
        assert_eq!(
            list("code", None).expect_err("relative").0,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            list("/no/such/folder/here", None).expect_err("missing").0,
            StatusCode::NOT_FOUND
        );
    }
}
