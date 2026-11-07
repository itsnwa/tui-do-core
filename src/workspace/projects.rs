//! Projects: `projects/<slug>.md`, each pointing at a directory on the home
//! machine. A task runs on another machine with a copy of it (see handoff).

use serde::{Deserialize, Serialize};

use super::store::Dir;
use super::tasks::text_or;
use super::{Error, Result, Workspace, dates, slug};
use crate::format::{Map, Value};

const PROJECT_KEYS: [&str; 4] = ["title", "muted", "created", "directory"];

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub slug: String,
    pub title: String,
    pub muted: bool,
    pub created: String,
    pub directory: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateProject {
    pub title: String,
    pub directory: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UpdateProject {
    pub title: Option<String>,
    pub muted: Option<bool>,
    pub directory: Option<String>,
}

fn summarize(slug: &str, data: &Map, content: Option<String>) -> Project {
    Project {
        slug: slug.to_string(),
        title: text_or(data.get("title"), slug),
        muted: matches!(data.get("muted"), Some(Value::Bool(true))),
        created: text_or(data.get("created"), ""),
        directory: text_or(data.get("directory"), ""),
        content,
    }
}

fn ordered(data: Map) -> Map {
    let mut out = Map::new();
    for key in PROJECT_KEYS {
        if let Some(v) = data.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    for (k, v) in data {
        if !PROJECT_KEYS.contains(&k.as_str()) {
            out.insert(k, v);
        }
    }
    out
}

impl Workspace {
    pub fn list_projects(&self) -> Result<Vec<Project>> {
        Ok(self
            .store
            .list(Dir::Projects)?
            .iter()
            .map(|e| summarize(&e.slug, &e.doc.data, None))
            .collect())
    }

    pub fn get_project(&self, slug: &str) -> Result<Project> {
        slug::validate(slug)?;
        let doc = self.store.read_existing(Dir::Projects, slug, "Project")?;
        Ok(summarize(slug, &doc.data, Some(doc.content)))
    }

    pub fn create_project(&self, req: CreateProject) -> Result<Project> {
        if req.title.trim().is_empty() {
            return Err(Error::Invalid("a project needs a title".into()));
        }
        let _guard = self.writing();
        let slug = slug::unique(&self.store, Dir::Projects, &slug::slugify(&req.title));
        let mut data = Map::new();
        data.insert("title".into(), req.title.into());
        data.insert("muted".into(), false.into());
        data.insert("created".into(), dates::now_iso().into());
        data.insert("directory".into(), req.directory.unwrap_or_default().into());
        self.store.write(Dir::Projects, &slug, &data, "")?;
        Ok(summarize(&slug, &data, None))
    }

    /// Update a project. A new title renames it (new slug), and its tasks are
    /// moved to the new slug.
    pub fn update_project(&self, slug: &str, req: UpdateProject) -> Result<Project> {
        slug::validate(slug)?;
        let _guard = self.writing();
        let doc = self.store.read_existing(Dir::Projects, slug, "Project")?;
        let mut data = doc.data.clone();
        if let Some(title) = &req.title {
            data.insert("title".into(), title.clone().into());
        }
        let muted = req
            .muted
            .unwrap_or(matches!(data.get("muted"), Some(Value::Bool(true))));
        data.insert("muted".into(), muted.into());
        let directory = req
            .directory
            .unwrap_or_else(|| text_or(data.get("directory"), ""));
        data.insert("directory".into(), directory.into());
        let data = ordered(data);

        let new_slug = match req.title.as_deref().map(slug::slugify) {
            Some(s) if !s.is_empty() && s != slug => s,
            _ => slug.to_string(),
        };
        if new_slug != slug && self.store.exists(Dir::Projects, &new_slug) {
            return Err(Error::Invalid(format!(
                "a project with the slug \"{new_slug}\" already exists"
            )));
        }
        self.store
            .write(Dir::Projects, &new_slug, &data, &doc.content)?;
        if new_slug != slug {
            // Move the tasks before removing the old file, so an interruption
            // never leaves tasks pointing at a project that doesn't exist.
            for entry in self.store.list(Dir::Tasks)? {
                if entry.doc.data.get("project").and_then(Value::as_str) == Some(slug) {
                    let mut task = entry.doc.data.clone();
                    task.insert("project".into(), new_slug.clone().into());
                    self.write_task(&entry.slug, &task, &entry.doc.content)?;
                }
            }
            self.store.remove(Dir::Projects, slug)?;
        }
        Ok(summarize(&new_slug, &data, None))
    }

    pub fn delete_project(&self, slug: &str) -> Result<()> {
        slug::validate(slug)?;
        let _guard = self.writing();
        if self.store.remove(Dir::Projects, slug)? {
            Ok(())
        } else {
            Err(Error::NotFound(format!("Project not found: {slug}")))
        }
    }
}
