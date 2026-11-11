//! Projects: `projects/<slug>.md`, each pointing at a directory on the code
//! machine and/or a git remote. A task runs on another machine with a copy of
//! it, from the code machine or the remote (see handoff).

use serde::{Deserialize, Serialize};

use super::store::Dir;
use super::tasks::text_or;
use super::{Error, Result, Workspace, dates, slug};
use crate::format::{Map, Value};

const PROJECT_KEYS: [&str; 6] = [
    "title",
    "muted",
    "archived",
    "created",
    "directory",
    "remote",
];

#[derive(Clone, Debug, Serialize)]
pub struct Project {
    pub slug: String,
    pub title: String,
    pub muted: bool,
    pub archived: bool,
    pub created: String,
    pub directory: String,
    /// The git remote the code can be fetched from (e.g. its GitHub URL).
    pub remote: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateProject {
    pub title: String,
    pub directory: Option<String>,
    pub remote: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UpdateProject {
    pub title: Option<String>,
    pub muted: Option<bool>,
    pub archived: Option<bool>,
    pub directory: Option<String>,
    pub remote: Option<String>,
}

fn summarize(slug: &str, data: &Map, content: Option<String>) -> Project {
    Project {
        slug: slug.to_string(),
        title: text_or(data.get("title"), slug),
        muted: matches!(data.get("muted"), Some(Value::Bool(true))),
        archived: matches!(data.get("archived"), Some(Value::Bool(true))),
        created: text_or(data.get("created"), ""),
        directory: text_or(data.get("directory"), ""),
        remote: text_or(data.get("remote"), ""),
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
        let remote = req.remote.unwrap_or_default();
        if !remote.trim().is_empty() {
            data.insert("remote".into(), remote.trim().into());
        }
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
        let archived = req
            .archived
            .unwrap_or(matches!(data.get("archived"), Some(Value::Bool(true))));
        data.insert("archived".into(), archived.into());
        let directory = req
            .directory
            .unwrap_or_else(|| text_or(data.get("directory"), ""));
        data.insert("directory".into(), directory.into());
        match req.remote.as_deref().map(str::trim) {
            Some("") => {
                data.shift_remove("remote");
            }
            Some(remote) => {
                data.insert("remote".into(), remote.into());
            }
            None => {}
        }
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

    /// Delete a project with its tasks, and their plans and attachments.
    pub fn delete_project(&self, slug: &str) -> Result<()> {
        slug::validate(slug)?;
        let _guard = self.writing();
        if !self.store.exists(Dir::Projects, slug) {
            return Err(Error::NotFound(format!("Project not found: {slug}")));
        }
        // The tasks go before the project file, so an interrupted delete can
        // be run again to finish the job.
        for entry in self.store.list(Dir::Tasks)? {
            if entry.doc.data.get("project").and_then(Value::as_str) == Some(slug) {
                self.remove_task(&entry.slug)?;
            }
        }
        self.store.remove(Dir::Projects, slug)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::tasks::{CreateTask, TaskFilter};
    use super::*;

    fn workspace() -> Workspace {
        let root = std::env::temp_dir().join(format!("tui-do-projects-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).expect("dir");
        Workspace::open(root).expect("workspace")
    }

    fn project(ws: &Workspace, title: &str) -> Project {
        ws.create_project(CreateProject {
            title: title.into(),
            ..CreateProject::default()
        })
        .expect("project")
    }

    /// A task in the project, with a plan and a stored attachment. Returns its slug.
    fn add_task(ws: &Workspace, title: &str, project: &str) -> String {
        let task = ws
            .create_task(CreateTask {
                title: title.into(),
                project: Some(project.into()),
                ..CreateTask::default()
            })
            .expect("task")
            .task;
        ws.write_task_plan(&task.id, "The plan").expect("plan");
        let attachments = ws.attachments_dir(&task.slug);
        std::fs::create_dir_all(&attachments).expect("dir");
        std::fs::write(attachments.join("shot.png"), "png").expect("write");
        task.slug
    }

    fn set_archived(ws: &Workspace, slug: &str, archived: bool) -> Project {
        ws.update_project(
            slug,
            UpdateProject {
                archived: Some(archived),
                ..UpdateProject::default()
            },
        )
        .expect("update")
    }

    #[test]
    fn archiving_should_be_reversible_and_keep_tasks() {
        let ws = workspace();
        let p = project(&ws, "Old thing");
        add_task(&ws, "Keep me", &p.slug);
        assert!(!p.archived);

        assert!(set_archived(&ws, &p.slug, true).archived);
        assert!(ws.get_project(&p.slug).expect("get").archived);
        assert_eq!(
            ws.list_tasks(&TaskFilter::default()).expect("tasks")[0].project,
            p.slug
        );

        assert!(!set_archived(&ws, &p.slug, false).archived);
    }

    #[test]
    fn updating_other_fields_should_leave_a_project_archived() {
        let ws = workspace();
        let p = project(&ws, "Old thing");
        set_archived(&ws, &p.slug, true);
        let updated = ws
            .update_project(
                &p.slug,
                UpdateProject {
                    muted: Some(true),
                    ..UpdateProject::default()
                },
            )
            .expect("update");
        assert!(updated.archived && updated.muted);
    }

    #[test]
    fn deleting_should_remove_only_its_own_tasks_plans_and_attachments() {
        let ws = workspace();
        let gone = project(&ws, "Gone");
        let other = project(&ws, "Other");
        let removed = add_task(&ws, "Removed", &gone.slug);
        let kept = add_task(&ws, "Kept", &other.slug);
        assert_eq!(ws.list_plans().expect("plans").len(), 2);

        ws.delete_project(&gone.slug).expect("delete");

        assert!(ws.get_project(&gone.slug).is_err());
        assert!(ws.get_project(&other.slug).is_ok());
        let tasks = ws.list_tasks(&TaskFilter::default()).expect("tasks");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].slug, kept);
        assert_eq!(ws.list_plans().expect("plans").len(), 1);
        assert!(!ws.attachments_dir(&removed).exists());
        assert!(ws.attachments_dir(&kept).join("shot.png").exists());
    }

    #[test]
    fn deleting_a_missing_project_should_be_not_found() {
        let ws = workspace();
        assert!(matches!(ws.delete_project("nope"), Err(Error::NotFound(_))));
    }
}
