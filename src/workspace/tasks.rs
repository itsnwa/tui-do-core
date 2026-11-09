//! Tasks: `tasks/<slug>.md`.
//!
//! Frontmatter keys are written in the app's order (`TASK_KEYS`); any other
//! keys a file has are kept after them.

use std::cmp::Ordering;

use base64::Engine;
use rand::RngExt;
use serde::{Deserialize, Serialize, Serializer};
use serde_json::json;

use super::dates;
use super::store::{Dir, Entry};
use super::{Error, Result, Workspace, slug};
use crate::format::{Document, Map, Value};

const TASK_KEYS: [&str; 12] = [
    "id",
    "title",
    "state",
    "project",
    "due",
    "recurrence",
    "plan",
    "position",
    "completed",
    "created",
    "attachments",
    "device",
];
pub const STATES: [&str; 3] = ["not-started", "in-progress", "completed"];

/// A task as listed. Matches the app's `TaskSummary`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Task {
    pub slug: String,
    pub id: String,
    pub title: String,
    pub state: String,
    pub project: String,
    pub due: Option<String>,
    pub recurrence: Option<String>,
    pub plan: Option<String>,
    #[serde(serialize_with = "js_number")]
    pub position: f64,
    pub completed: Option<String>,
    pub created: String,
    pub attachments: Vec<String>,
    /// The fleet device the task runs on (chosen when starting it); absent = the home machine.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

/// A task with its description (and, from `get`, the same text as `content`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskDetail {
    #[serde(flatten)]
    pub task: Task,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

/// JSON numbers the way JS prints them: integral values without a fraction.
pub fn js_number<S: Serializer>(n: &f64, s: S) -> std::result::Result<S::Ok, S::Error> {
    if n.fract() == 0.0 && n.abs() < 9_007_199_254_740_992.0 {
        s.serialize_i64(*n as i64)
    } else {
        s.serialize_f64(*n)
    }
}

/// Distinguishes a field that's absent (`None`) from one set to `null` (`Some(None)`).
pub fn double_option<'de, D, T>(de: D) -> std::result::Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(de).map(Some)
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TaskFilter {
    pub project: Option<String>,
    pub state: Option<String>,
    pub due: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CreateTask {
    pub title: String,
    pub project: Option<String>,
    pub due: Option<String>,
    pub recurrence: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct UpdateTask {
    pub title: Option<String>,
    pub state: Option<String>,
    pub project: Option<String>,
    #[serde(deserialize_with = "double_option")]
    pub due: Option<Option<String>>,
    #[serde(deserialize_with = "double_option")]
    pub recurrence: Option<Option<String>>,
    #[serde(deserialize_with = "double_option")]
    pub plan: Option<Option<String>>,
    pub description: Option<String>,
    /// The device to run on; `null` = the home machine.
    #[serde(deserialize_with = "double_option")]
    pub device: Option<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct ReorderResult {
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'static str>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlanSnapshot {
    pub slug: String,
    pub title: String,
    pub task: Option<String>,
    pub created: String,
    pub content: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachmentSnapshot {
    pub file_path: String,
    pub data_base64: String,
}

/// What `delete` returns, and `restore` takes back to undo it.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeletedTask {
    pub task: TaskDetail,
    #[serde(default)]
    pub plan_snapshot: Option<PlanSnapshot>,
    #[serde(default)]
    pub attachment_snapshots: Vec<AttachmentSnapshot>,
}

/// `value ?? default` as text.
pub(super) fn text_or(v: Option<&Value>, default: &str) -> String {
    match v {
        None | Some(Value::Null) => default.to_string(),
        Some(v) => v.to_js_string(),
    }
}

/// `value ?? null` as text.
pub(super) fn opt_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_js_string()),
    }
}

fn summarize(slug: &str, data: &Map) -> Task {
    Task {
        slug: slug.to_string(),
        id: text_or(data.get("id"), ""),
        title: text_or(data.get("title"), slug),
        state: text_or(data.get("state"), "not-started"),
        project: text_or(data.get("project"), ""),
        due: dates::due_of(data.get("due")),
        recurrence: dates::valid_recurrence(data.get("recurrence")).map(str::to_string),
        plan: opt_text(data.get("plan")),
        position: match data.get("position") {
            Some(Value::Number(n)) => *n,
            _ => 0.0,
        },
        completed: opt_text(data.get("completed")),
        created: text_or(data.get("created"), ""),
        attachments: match data.get("attachments") {
            Some(Value::Seq(items)) => items.iter().map(Value::to_js_string).collect(),
            _ => Vec::new(),
        },
        device: opt_text(data.get("device")).filter(|d| !d.is_empty()),
    }
}

fn detail(slug: &str, doc: &Document, with_content: bool) -> TaskDetail {
    TaskDetail {
        task: summarize(slug, &doc.data),
        description: doc.content.clone(),
        content: with_content.then(|| doc.content.clone()),
    }
}

/// The app's `sortTasks`: open tasks first (dated by due date, then undated),
/// then completed tasks, most recently completed first; ties by position.
fn compare(a: &Task, b: &Task) -> Ordering {
    let by_position = || {
        a.position
            .partial_cmp(&b.position)
            .unwrap_or(Ordering::Equal)
    };
    let (a_done, b_done) = (a.state == "completed", b.state == "completed");
    match (a_done, b_done) {
        (false, true) => Ordering::Less,
        (true, false) => Ordering::Greater,
        (false, false) => match (&a.due, &b.due) {
            (Some(x), Some(y)) => x.cmp(y).then_with(by_position),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => by_position(),
        },
        (true, true) => match (&a.completed, &b.completed) {
            (Some(x), Some(y)) if !x.is_empty() && !y.is_empty() => y.cmp(x).then_with(by_position),
            (Some(x), _) if !x.is_empty() => Ordering::Less,
            (_, Some(y)) if !y.is_empty() => Ordering::Greater,
            _ => by_position(),
        },
    }
}

fn plan_slug_words() -> (&'static str, &'static str) {
    const ADJECTIVES: [&str; 84] = [
        "vibranium",
        "adamantium",
        "cosmic",
        "mystic",
        "quantum",
        "savage",
        "mighty",
        "uncanny",
        "infinite",
        "astral",
        "shadow",
        "crimson",
        "frozen",
        "blazing",
        "silent",
        "ancient",
        "eternal",
        "phantom",
        "stellar",
        "primal",
        "golden",
        "dark",
        "wild",
        "storm",
        "sovereign",
        "celestial",
        "unstoppable",
        "invincible",
        "sinister",
        "spectral",
        "shattered",
        "relentless",
        "hallowed",
        "radiant",
        "luminous",
        "fractured",
        "molten",
        "cursed",
        "vengeful",
        "arcane",
        "feral",
        "volatile",
        "ruthless",
        "temporal",
        "primordial",
        "forsaken",
        "colossal",
        "dire",
        "rampant",
        "cataclysmic",
        "verdant",
        "obsidian",
        "scarlet",
        "azure",
        "emerald",
        "titanium",
        "orbital",
        "nocturnal",
        "ferocious",
        "resolute",
        "defiant",
        "infernal",
        "glacial",
        "thunderous",
        "warped",
        "enchanted",
        "chaotic",
        "supreme",
        "brutal",
        "lethal",
        "ominous",
        "valiant",
        "noble",
        "wretched",
        "hollow",
        "iron",
        "silver",
        "ashen",
        "vivid",
        "surging",
        "twin",
        "last",
        "first",
        "fallen",
    ];
    const NOUNS: [&str; 105] = [
        "panther",
        "phoenix",
        "hulk",
        "thor",
        "widow",
        "hawk",
        "fury",
        "groot",
        "rocket",
        "nebula",
        "gamora",
        "drax",
        "loki",
        "odin",
        "stark",
        "banner",
        "thanos",
        "ultron",
        "vision",
        "wanda",
        "rogue",
        "logan",
        "storm",
        "cyclops",
        "blade",
        "nova",
        "venom",
        "doom",
        "surfer",
        "sentry",
        "magneto",
        "xavier",
        "colossus",
        "cable",
        "bishop",
        "mystique",
        "nightcrawler",
        "beast",
        "angel",
        "jubilee",
        "dazzler",
        "forge",
        "havok",
        "polaris",
        "warlock",
        "cannonball",
        "sunspot",
        "magik",
        "domino",
        "deadpool",
        "punisher",
        "elektra",
        "kingpin",
        "bullseye",
        "daredevil",
        "cage",
        "fist",
        "mantis",
        "valkyrie",
        "heimdall",
        "sif",
        "hela",
        "ego",
        "dormammu",
        "mephisto",
        "galactus",
        "kang",
        "modok",
        "taskmaster",
        "crossbones",
        "zemo",
        "strucker",
        "ronan",
        "korath",
        "yondu",
        "ravager",
        "skrull",
        "kree",
        "chitauri",
        "sentinel",
        "watcher",
        "herald",
        "devourer",
        "destroyer",
        "reaper",
        "specter",
        "titan",
        "oracle",
        "raptor",
        "tempest",
        "corsair",
        "nomad",
        "ronin",
        "prowler",
        "wraith",
        "spider",
        "falcon",
        "wasp",
        "ant",
        "leopard",
        "wolverine",
        "sabretooth",
        "juggernaut",
        "absorber",
        "abomination",
    ];
    let mut rng = rand::rng();
    (
        ADJECTIVES[rng.random_range(0..ADJECTIVES.len())],
        NOUNS[rng.random_range(0..NOUNS.len())],
    )
}

impl Workspace {
    pub(super) fn write_task(&self, slug: &str, data: &Map, body: &str) -> Result<()> {
        let mut out = Map::new();
        for key in TASK_KEYS {
            if let Some(v) = data.get(key) {
                let v = if key == "due" {
                    dates::due_of(Some(v)).map_or(Value::Null, Value::String)
                } else {
                    v.clone()
                };
                out.insert(key.to_string(), v);
            }
        }
        for (k, v) in data {
            if !TASK_KEYS.contains(&k.as_str()) {
                out.insert(k.clone(), v.clone());
            }
        }
        self.store.write(Dir::Tasks, slug, &out, body)
    }

    fn task_doc(&self, slug: &str) -> Result<Document> {
        slug::validate(slug)?;
        self.store.read_existing(Dir::Tasks, slug, "Task")
    }

    fn task_entries(&self) -> Result<Vec<Entry>> {
        self.store.list(Dir::Tasks)
    }

    pub fn list_tasks(&self, filter: &TaskFilter) -> Result<Vec<Task>> {
        let non_empty = |s: &Option<String>| s.clone().filter(|s| !s.is_empty());
        let (project, state, due) = (
            non_empty(&filter.project),
            non_empty(&filter.state),
            non_empty(&filter.due),
        );
        let mut tasks: Vec<Task> = self
            .task_entries()?
            .iter()
            .map(|e| summarize(&e.slug, &e.doc.data))
            .filter(|t| project.as_ref().is_none_or(|p| &t.project == p))
            .filter(|t| state.as_ref().is_none_or(|s| &t.state == s))
            .filter(|t| {
                due.as_ref().is_none_or(|d| {
                    t.due.as_ref() == Some(d)
                        || dates::is_recurring_on_date(
                            t.due.as_deref().unwrap_or(""),
                            t.recurrence.as_deref(),
                            d,
                        )
                })
            })
            .collect();
        tasks.sort_by(compare);
        Ok(tasks)
    }

    pub fn get_task(&self, slug: &str) -> Result<TaskDetail> {
        Ok(detail(slug, &self.task_doc(slug)?, true))
    }

    /// Find a task by short ID (`58`, `#58`, `26-058`; bare numbers are this
    /// year's) or, failing that, by slug.
    pub fn find_task(&self, reference: &str) -> Result<Option<String>> {
        let mut id = reference.trim().trim_start_matches('#').to_string();
        if !id.is_empty() && id.chars().all(|c| c.is_ascii_digit()) {
            id = format!("{}-{id:0>3}", dates::id_year_prefix());
        }
        let entries = self.task_entries()?;
        if let Some(e) = entries
            .iter()
            .find(|e| e.doc.data.get("id").and_then(Value::as_str) == Some(id.as_str()))
        {
            return Ok(Some(e.slug.clone()));
        }
        let by_slug = slug::validate(reference).is_ok() && self.store.exists(Dir::Tasks, reference);
        Ok(by_slug.then(|| reference.to_string()))
    }

    fn next_id(&self, entries: &[Entry]) -> String {
        let prefix = dates::id_year_prefix();
        let max = entries
            .iter()
            .filter_map(|e| e.doc.data.get("id").and_then(Value::as_str))
            .filter_map(|id| id.strip_prefix(&prefix)?.strip_prefix('-'))
            .filter_map(|rest| {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                digits.parse::<u64>().ok()
            })
            .max()
            .unwrap_or(0);
        format!("{prefix}-{:03}", max + 1)
    }

    fn next_position(entries: &[Entry]) -> f64 {
        let min = entries
            .iter()
            .filter_map(|e| match e.doc.data.get("position") {
                Some(Value::Number(n)) => Some(*n),
                _ => None,
            })
            .fold(0.0_f64, f64::min);
        min - 1.0
    }

    pub fn create_task(&self, req: CreateTask) -> Result<TaskDetail> {
        if req.title.trim().is_empty() {
            return Err(Error::Invalid("a task needs a title".into()));
        }
        let due = req.due.filter(|d| !d.is_empty());
        if let Some(d) = &due
            && !dates::is_date(d)
        {
            return Err(Error::Invalid(format!("due must be YYYY-MM-DD, got {d:?}")));
        }
        let _guard = self.writing();
        let entries = self.task_entries()?;
        let slug = slug::unique(&self.store, Dir::Tasks, &slug::slugify(&req.title));
        let recurrence = req
            .recurrence
            .filter(|r| dates::RECURRENCES.contains(&r.as_str()));
        let mut data = Map::new();
        data.insert("id".into(), self.next_id(&entries).into());
        data.insert("title".into(), req.title.into());
        data.insert("state".into(), "not-started".into());
        data.insert("project".into(), req.project.unwrap_or_default().into());
        data.insert("due".into(), due.into());
        data.insert("recurrence".into(), recurrence.into());
        data.insert("plan".into(), Value::Null);
        data.insert("position".into(), Self::next_position(&entries).into());
        data.insert("completed".into(), Value::Null);
        data.insert("created".into(), dates::now_iso().into());
        data.insert("attachments".into(), Value::Seq(Vec::new()));
        let body = req.description.unwrap_or_default();
        self.write_task(&slug, &data, &body)?;
        Ok(TaskDetail {
            task: summarize(&slug, &data),
            description: body,
            content: None,
        })
    }

    pub fn update_task(&self, slug: &str, req: UpdateTask) -> Result<TaskDetail> {
        let _guard = self.writing();
        let doc = self.task_doc(slug)?;
        let mut data = doc.data.clone();
        let current_recurrence = dates::valid_recurrence(data.get("recurrence"));
        let new_recurrence = match &req.recurrence {
            Some(r) => r
                .as_deref()
                .and_then(|r| dates::RECURRENCES.iter().copied().find(|x| *x == r)),
            None => current_recurrence,
        };
        let old_state = text_or(data.get("state"), "not-started");
        let old_due = dates::due_of(data.get("due"));
        let mut completed = data.get("completed").cloned().unwrap_or(Value::Null);
        let mut state = req.state.clone().unwrap_or_else(|| old_state.clone());
        let mut due = match &req.due {
            Some(d) => d.clone().filter(|d| !d.is_empty()),
            None => old_due.clone(),
        };
        if let Some(d) = &due
            && !dates::is_date(d)
            && req.due.is_some()
        {
            return Err(Error::Invalid(format!("due must be YYYY-MM-DD, got {d:?}")));
        }
        let completing = req.state.as_deref() == Some("completed") && old_state != "completed";
        let next_due = match (completing, new_recurrence, &old_due) {
            (true, Some(rec), Some(d)) => dates::next_occurrence(d, rec),
            _ => None,
        };
        if let Some(next) = next_due {
            // A recurring task moves to its next date instead of completing
            due = Some(next);
            state = "not-started".into();
            completed = Value::Null;
        } else if completing {
            completed = dates::now_iso().into();
        } else if req.state.as_deref().is_some_and(|s| s != "completed") {
            completed = Value::Null;
        }

        data.insert(
            "id".into(),
            data.get("id")
                .cloned()
                .filter(|v| !v.is_null())
                .unwrap_or_else(|| "".into()),
        );
        if let Some(title) = req.title {
            data.insert("title".into(), title.into());
        }
        data.insert("state".into(), state.into());
        let project = req
            .project
            .unwrap_or_else(|| text_or(data.get("project"), ""));
        data.insert("project".into(), project.into());
        data.insert("due".into(), due.into());
        data.insert(
            "recurrence".into(),
            new_recurrence.map(str::to_string).into(),
        );
        let plan = match req.plan {
            Some(p) => p.into(),
            None => data.get("plan").cloned().unwrap_or(Value::Null),
        };
        data.insert("plan".into(), plan);
        let position = match data.get("position") {
            Some(Value::Number(n)) => *n,
            _ => 0.0,
        };
        data.insert("position".into(), position.into());
        data.insert("completed".into(), completed);
        if !matches!(data.get("attachments"), Some(Value::Seq(_))) {
            data.insert("attachments".into(), Value::Seq(Vec::new()));
        }
        match req.device {
            Some(Some(device)) if !device.is_empty() => {
                data.insert("device".into(), device.into());
            }
            Some(_) => {
                data.shift_remove("device");
            }
            None => {}
        }
        let body = req.description.unwrap_or(doc.content);
        self.write_task(slug, &data, &body)?;
        Ok(TaskDetail {
            task: summarize(slug, &data),
            description: body,
            content: None,
        })
    }

    /// Move a task up or down among open tasks with the same due date. Failures
    /// are reported in the result, not as errors (the app shows them inline).
    pub fn reorder_task(
        &self,
        slug: &str,
        up: bool,
        project: Option<&str>,
    ) -> Result<ReorderResult> {
        let _guard = self.writing();
        let filter = TaskFilter {
            project: project.map(str::to_string),
            ..TaskFilter::default()
        };
        let tasks = self.list_tasks(&filter)?;
        let fail = |error| {
            Ok(ReorderResult {
                success: false,
                error: Some(error),
            })
        };
        let Some(idx) = tasks.iter().position(|t| t.slug == slug) else {
            return fail("Task not found");
        };
        let adjacent = if up {
            idx.checked_sub(1)
        } else {
            Some(idx + 1)
        };
        let Some(adj) = adjacent.filter(|&i| i < tasks.len()) else {
            return fail("Already at boundary");
        };
        let (task, other) = (&tasks[idx], &tasks[adj]);
        let done = |t: &Task| t.state == "completed";
        if done(task) {
            return fail("Cannot reorder completed tasks");
        }
        if task.due != other.due {
            return fail("Cannot move across different due dates");
        }
        if done(task) != done(other) {
            return fail("Cannot move across completion status boundary");
        }
        let mut group: Vec<&Task> = tasks
            .iter()
            .filter(|t| t.due == task.due && done(t) == done(task))
            .collect();
        let (gi, ga) = (
            group.iter().position(|t| t.slug == task.slug).unwrap_or(0),
            group.iter().position(|t| t.slug == other.slug).unwrap_or(0),
        );
        group.swap(gi, ga);
        for (i, t) in group.iter().enumerate() {
            let doc = self.task_doc(&t.slug)?;
            let mut data = doc.data;
            data.insert("position".into(), (i as f64).into());
            self.write_task(&t.slug, &data, &doc.content)?;
        }
        Ok(ReorderResult {
            success: true,
            error: None,
        })
    }

    pub fn add_attachment(&self, slug: &str, file_path: &str) -> Result<TaskDetail> {
        self.edit_attachments(slug, |list| {
            if !list.iter().any(|p| p == file_path) {
                list.push(file_path.to_string());
            }
        })
    }

    /// Remove an attachment, given as stored or as another machine's path
    /// to the same stored file (see `local_attachment`).
    pub fn remove_attachment(&self, slug: &str, file_path: &str) -> Result<TaskDetail> {
        self.edit_attachments(slug, |list| {
            list.retain(|p| !super::same_attachment(p, file_path));
        })
    }

    fn edit_attachments(
        &self,
        slug: &str,
        edit: impl FnOnce(&mut Vec<String>),
    ) -> Result<TaskDetail> {
        let _guard = self.writing();
        let doc = self.task_doc(slug)?;
        let mut data = doc.data;
        let mut list = summarize(slug, &data).attachments;
        edit(&mut list);
        data.insert(
            "attachments".into(),
            Value::Seq(list.into_iter().map(Value::String).collect()),
        );
        self.write_task(slug, &data, &doc.content)?;
        Ok(TaskDetail {
            task: summarize(slug, &data),
            description: doc.content,
            content: None,
        })
    }

    /// Store an uploaded file under `attachments/<slug>/` and return its path.
    pub fn save_attachment_file(
        &self,
        slug: &str,
        file_name: &str,
        data_base64: &str,
    ) -> Result<String> {
        slug::validate(slug)?;
        let name = std::path::Path::new(file_name)
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| !n.starts_with('.'))
            .ok_or_else(|| Error::Invalid(format!("invalid file name {file_name:?}")))?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(data_base64)
            .map_err(|e| Error::Invalid(format!("attachment isn't valid base64: {e}")))?;
        let dir = self.attachments_dir(slug);
        std::fs::create_dir_all(&dir)?;
        let path = dir.join(name);
        std::fs::write(&path, bytes)?;
        Ok(path.display().to_string())
    }

    /// Delete a task with its plan and stored attachments, returning what's
    /// needed to undo it.
    pub fn delete_task(&self, slug: &str) -> Result<DeletedTask> {
        let _guard = self.writing();
        self.remove_task(slug)
    }

    /// [`Workspace::delete_task`], for a caller already holding the write lock.
    pub(super) fn remove_task(&self, slug: &str) -> Result<DeletedTask> {
        let doc = self.task_doc(slug)?;
        let task = detail(slug, &doc, true);
        let mut plan_snapshot = None;
        if let Some(plan) = &task.task.plan
            && slug::validate(plan).is_ok()
            && let Ok(Some(plan_doc)) = self.store.read(Dir::Plans, plan)
        {
            plan_snapshot = Some(PlanSnapshot {
                slug: plan.clone(),
                title: text_or(plan_doc.data.get("title"), plan),
                task: opt_text(plan_doc.data.get("task")),
                created: text_or(plan_doc.data.get("created"), ""),
                content: plan_doc.content,
            });
            self.store.remove(Dir::Plans, plan)?;
        }
        let attach_dir = self.attachments_dir(slug);
        let attachment_snapshots = task
            .task
            .attachments
            .iter()
            .filter(|p| std::path::Path::new(p).starts_with(&attach_dir))
            .filter_map(|p| {
                let bytes = std::fs::read(p).ok()?;
                Some(AttachmentSnapshot {
                    file_path: p.clone(),
                    data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
                })
            })
            .collect();
        let _ = std::fs::remove_dir_all(&attach_dir);
        self.store.remove(Dir::Tasks, slug)?;
        Ok(DeletedTask {
            task,
            plan_snapshot,
            attachment_snapshots,
        })
    }

    /// Undo [`Workspace::delete_task`].
    pub fn restore_task(&self, deleted: DeletedTask) -> Result<TaskDetail> {
        let _guard = self.writing();
        let t = &deleted.task.task;
        slug::validate(&t.slug)?;
        let mut data = Map::new();
        data.insert("id".into(), t.id.clone().into());
        data.insert("title".into(), t.title.clone().into());
        data.insert("state".into(), t.state.clone().into());
        data.insert("project".into(), t.project.clone().into());
        data.insert("due".into(), t.due.clone().into());
        data.insert("recurrence".into(), t.recurrence.clone().into());
        data.insert("plan".into(), t.plan.clone().into());
        data.insert("position".into(), t.position.into());
        data.insert("completed".into(), t.completed.clone().into());
        data.insert("created".into(), t.created.clone().into());
        data.insert(
            "attachments".into(),
            Value::Seq(t.attachments.iter().cloned().map(Value::String).collect()),
        );
        let body = deleted
            .task
            .content
            .clone()
            .unwrap_or_else(|| deleted.task.description.clone());
        self.write_task(&t.slug, &data, &body)?;
        if let Some(ps) = &deleted.plan_snapshot {
            slug::validate(&ps.slug)?;
            let mut plan = Map::new();
            plan.insert("title".into(), ps.title.clone().into());
            plan.insert("task".into(), ps.task.clone().into());
            let created = if ps.created.is_empty() {
                dates::now_iso()
            } else {
                ps.created.clone()
            };
            plan.insert("created".into(), created.into());
            self.store.write(Dir::Plans, &ps.slug, &plan, &ps.content)?;
        }
        let attach_dir = self.attachments_dir(&t.slug);
        for att in &deleted.attachment_snapshots {
            let path = std::path::Path::new(&att.file_path);
            // Only ever write back inside this task's attachment folder
            if !path.starts_with(&attach_dir) || att.file_path.contains("..") {
                continue;
            }
            if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(&att.data_base64) {
                std::fs::create_dir_all(&attach_dir)?;
                std::fs::write(path, bytes)?;
            }
        }
        Ok(TaskDetail {
            task: summarize(&t.slug, &data),
            description: body,
            content: None,
        })
    }

    fn generate_plan_slug(&self, data: &Map, slug: &str) -> String {
        let base = opt_text(data.get("id")).unwrap_or_else(|| slug.to_string());
        loop {
            let (adj, noun) = plan_slug_words();
            let candidate = format!("{base}-{adj}-{noun}");
            if !self.store.exists(Dir::Plans, &candidate) {
                return candidate;
            }
        }
    }

    fn task_not_found(reference: &str) -> serde_json::Value {
        json!({ "error": format!("Task not found with ID \"{reference}\". Use list_tasks to see available tasks.") })
    }

    /// Start working on a task (agent workflow): marks it in progress, reserves
    /// a plan slug, and returns its description, attachments and plan.
    pub fn start_task(
        &self,
        reference: &str,
        expected_project: Option<&str>,
    ) -> Result<serde_json::Value> {
        let _guard = self.writing();
        let Some(slug) = self.find_task(reference)? else {
            return Ok(Self::task_not_found(reference));
        };
        let doc = self.task_doc(&slug)?;
        let task = summarize(&slug, &doc.data);
        if let Some(expected) = expected_project.filter(|p| !p.is_empty())
            && !task.project.is_empty()
            && task.project != expected
        {
            let alternatives: Vec<_> = self
                .list_tasks(&TaskFilter {
                    project: Some(expected.to_string()),
                    ..TaskFilter::default()
                })?
                .into_iter()
                .filter(|t| t.state != "completed")
                .take(5)
                .map(|t| json!({ "id": t.id, "title": t.title, "slug": t.slug }))
                .collect();
            return Ok(json!({
                "warning": "PROJECT_MISMATCH",
                "message": format!("Task {} \"{}\" belongs to project \"{}\", not \"{expected}\".", task.id, task.title, task.project),
                "task": { "id": task.id, "title": task.title, "project": task.project },
                "suggestion": format!("Did you mean one of these tasks from \"{expected}\"?"),
                "alternatives": alternatives,
            }));
        }
        let mut data = doc.data.clone();
        data.insert("state".into(), "in-progress".into());
        data.insert("completed".into(), Value::Null);
        let plan = match opt_text(data.get("plan")).filter(|p| !p.is_empty()) {
            Some(plan) => plan,
            None => {
                let plan = self.generate_plan_slug(&data, &slug);
                data.insert("plan".into(), plan.clone().into());
                plan
            }
        };
        self.write_task(&slug, &data, &doc.content)?;
        let plan_content = if slug::validate(&plan).is_ok() {
            self.store
                .read(Dir::Plans, &plan)?
                .map(|d| d.content)
                .unwrap_or_default()
        } else {
            String::new()
        };
        let task = summarize(&slug, &data);
        Ok(json!({
            "status": "started",
            "task": {
                "slug": slug, "id": task.id, "title": task.title, "state": task.state, "project": task.project,
                "due": task.due, "description": doc.content, "attachments": task.attachments,
            },
            "plan": { "slug": plan, "content": plan_content },
        }))
    }

    /// Complete a task (agent workflow). The summary goes into the task's plan
    /// as "## Completion Summary"; a recurring task moves to its next date.
    pub fn complete_task(
        &self,
        reference: &str,
        summary: Option<&str>,
    ) -> Result<serde_json::Value> {
        let _guard = self.writing();
        let Some(slug) = self.find_task(reference)? else {
            return Ok(json!({ "error": format!("Task not found with ID \"{reference}\"") }));
        };
        let doc = self.task_doc(&slug)?;
        let task = summarize(&slug, &doc.data);
        let Some(summary) = summary.filter(|s| !s.trim().is_empty()) else {
            return Ok(json!({
                "error": "SUMMARY_REQUIRED",
                "message": "A completion summary is required to mark a task as completed. Please provide a summary describing what was done, how it works, and any important decisions made. Call complete_task again with the summary parameter.",
                "task": { "id": task.id, "title": task.title, "state": task.state },
            }));
        };
        let mut data = doc.data.clone();
        let plan = match task.plan.clone().filter(|p| !p.is_empty()) {
            Some(plan) => plan,
            None => {
                let plan = self.generate_plan_slug(&data, &slug);
                data.insert("plan".into(), plan.clone().into());
                plan
            }
        };
        // The plan is written first, so the summary is visible before the state changes
        self.append_completion_summary(&plan, &slug, &task.title, summary)?;
        let next_due = match (task.recurrence.as_deref(), task.due.as_deref()) {
            (Some(rec), Some(due)) => dates::next_occurrence(due, rec),
            _ => None,
        };
        if let Some(next) = next_due {
            data.insert("state".into(), "not-started".into());
            data.insert("due".into(), next.clone().into());
            data.insert("completed".into(), Value::Null);
            self.write_task(&slug, &data, &doc.content)?;
            return Ok(json!({
                "status": "recurring_advanced",
                "task": {
                    "slug": slug, "id": task.id, "title": task.title, "state": "not-started",
                    "project": task.project, "nextDue": next, "recurrence": task.recurrence,
                },
            }));
        }
        data.insert("state".into(), "completed".into());
        data.insert("completed".into(), dates::now_iso().into());
        self.write_task(&slug, &data, &doc.content)?;
        Ok(json!({
            "status": "completed",
            "task": { "slug": slug, "id": task.id, "title": task.title, "state": "completed", "project": task.project },
        }))
    }

    /// Replace the plan linked to a task (agent workflow), reserving a plan
    /// slug if the task has none.
    pub fn write_task_plan(&self, reference: &str, content: &str) -> Result<serde_json::Value> {
        let _guard = self.writing();
        let Some(slug) = self.find_task(reference)? else {
            return Ok(json!({ "error": format!("Task not found with ID \"{reference}\"") }));
        };
        let doc = self.task_doc(&slug)?;
        let title = text_or(doc.data.get("title"), &slug);
        let plan = match opt_text(doc.data.get("plan")).filter(|p| !p.is_empty()) {
            Some(plan) => plan,
            None => {
                let mut data = doc.data.clone();
                let plan = self.generate_plan_slug(&data, &slug);
                data.insert("plan".into(), plan.clone().into());
                self.write_task(&slug, &data, &doc.content)?;
                plan
            }
        };
        slug::validate(&plan)?;
        let data = match self.store.read(Dir::Plans, &plan)? {
            Some(existing) => existing.data,
            None => super::plans::new_plan_data(&title, &slug),
        };
        self.store.write(Dir::Plans, &plan, &data, content)?;
        Ok(json!({ "status": "updated", "plan": { "slug": plan, "task": slug } }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: &str, due: Option<&str>, completed: Option<&str>, position: f64) -> Task {
        Task {
            state: state.into(),
            due: due.map(Into::into),
            completed: completed.map(Into::into),
            position,
            ..Task::default()
        }
    }

    #[test]
    fn open_dated_tasks_should_sort_before_undated() {
        assert_eq!(
            compare(
                &task("not-started", Some("2026-01-01"), None, 5.0),
                &task("not-started", None, None, 0.0)
            ),
            Ordering::Less
        );
    }

    #[test]
    fn completed_tasks_should_sort_most_recent_first() {
        let older = task("completed", None, Some("2026-01-01T00:00:00.000Z"), 0.0);
        let newer = task("completed", None, Some("2026-02-01T00:00:00.000Z"), 0.0);
        assert_eq!(compare(&newer, &older), Ordering::Less);
    }

    #[test]
    fn states_should_list_the_three_states() {
        assert!(STATES.contains(&"in-progress"));
    }
}
