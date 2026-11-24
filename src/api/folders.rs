//! `GET /v0/fs/dirs`: the folders in a folder on this machine, for picking a
//! project's folder from another machine (through `/v0/devices/{id}/fs/dirs`).
//! `POST /v0/fs/repo`: make a project's folder here as a git repo, for the
//! Assistant setting up a project on this machine.

use std::path::{Path, PathBuf};

use axum::Json;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use serde::{Deserialize, Serialize};

use super::{AppState, error};
use crate::session::expand_home;

/// More than anyone scrolls through; the rest are left out.
const MAX_ENTRIES: usize = 2000;

pub fn routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v0/fs/dirs", get(dirs))
        .route("/v0/fs/repo", post(repo))
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

#[derive(Deserialize)]
struct RepoRequest {
    /// The folder, absolute or `~/…`.
    path: String,
    /// A git URL to clone into it, instead of starting a new repo.
    #[serde(default)]
    clone: Option<String>,
    /// `private` or `public`: create a GitHub repo for a new one and push.
    #[serde(default)]
    github: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Repo {
    /// The folder, as `~/…` when it's in the home folder.
    path: String,
    /// Its `origin`, if it has one.
    remote: Option<String>,
    /// Whether the repo was made now (not there already).
    created: bool,
}

async fn repo(Json(req): Json<RepoRequest>) -> Response {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    match tokio::task::spawn_blocking(move || make_repo(&req, home.as_deref())).await {
        Ok(Ok(repo)) => Json(repo).into_response(),
        Ok(Err(message)) => error(StatusCode::BAD_REQUEST, &message),
        Err(err) => error(StatusCode::INTERNAL_SERVER_ERROR, &err.to_string()),
    }
}

/// Make `req.path` a git repo: a clone of `req.clone`, or a new one (with a
/// GitHub repo if asked). A folder that's a repo already is left as it is.
fn make_repo(req: &RepoRequest, home: Option<&Path>) -> Result<Repo, String> {
    use crate::handoff::{git, github};
    let dir = expand_home(req.path.trim());
    if !dir.is_absolute() {
        return Err(format!("{} isn't an absolute path", req.path));
    }
    let clone = req
        .clone
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty());
    let github = req
        .github
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty());
    if clone.is_some() && github.is_some() {
        return Err("a clone has its remote already; github is for a new repo".into());
    }
    let existed = dir.join(".git").exists();
    if !existed {
        let empty = std::fs::read_dir(&dir).map_or(true, |mut entries| entries.next().is_none());
        match clone {
            Some(_) if !empty => {
                return Err(format!(
                    "{} is there already and isn't empty",
                    shown(&dir, home)
                ));
            }
            Some(url) => git::clone(url, &dir).map_err(|e| format!("{e:#}"))?,
            None => {
                std::fs::create_dir_all(&dir)
                    .map_err(|e| format!("can't create {}: {e}", shown(&dir, home)))?;
                git::init(&dir).map_err(|e| format!("{e:#}"))?;
            }
        }
    }
    if let Some(visibility) = github {
        if git::origin(&dir).is_some() {
            return Err(format!(
                "{} has a remote already; leave github out to use it",
                shown(&dir, home)
            ));
        }
        github::create_repo(&dir, visibility).map_err(|e| format!("{e:#}"))?;
    }
    Ok(Repo {
        path: shown(&dir, home),
        remote: git::origin(&dir),
        created: !existed,
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

    fn repo_request(path: &Path, clone: Option<&str>) -> RepoRequest {
        RepoRequest {
            path: path.display().to_string(),
            clone: clone.map(str::to_string),
            github: None,
        }
    }

    #[test]
    fn a_new_repo_should_get_a_first_commit_and_be_left_alone_after() {
        let root = folder();
        let dir = root.join("new/project");
        let made = make_repo(&repo_request(&dir, None), None).expect("made");
        assert!(made.created && made.remote.is_none());
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git");
        assert!(head.status.success());
        let again = make_repo(&repo_request(&dir, None), None).expect("again");
        assert!(!again.created);
        std::fs::remove_dir_all(root).expect("clean up");
    }

    #[test]
    fn a_clone_should_take_the_remote_and_not_fill_a_full_folder() {
        let root = folder();
        let source = root.join("source");
        make_repo(&repo_request(&source, None), None).expect("source");
        let url = source.display().to_string();
        let dir = root.join("clone");
        let cloned = make_repo(&repo_request(&dir, Some(&url)), None).expect("cloned");
        assert_eq!(cloned.remote.as_deref(), Some(url.as_str()));
        assert!(make_repo(&repo_request(&root.join("c"), Some(&url)), None).is_ok());
        assert!(make_repo(&repo_request(&root, Some(&url)), None).is_err());
        let mut both = repo_request(&root.join("both"), Some(&url));
        both.github = Some("private".into());
        assert!(make_repo(&both, None).is_err());
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
