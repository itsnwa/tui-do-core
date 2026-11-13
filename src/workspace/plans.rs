//! Plans: `plans/<slug>.md`, usually linked from a task's `plan` key.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use super::store::Dir;
use super::tasks::{double_option, opt_text, text_or};
use super::{Error, Result, Workspace, dates, slug};
use crate::format::{Map, Value};

const PLAN_KEYS: [&str; 3] = ["title", "task", "created"];
const SUMMARY_HEADING: &str = "## Completion Summary\n\n";

/// How many runs a recurring task's summary keeps.
const MAX_RUNS: usize = 30;

/// Where a run starts in a recurring task's summary: `### 2026-11-14 09:00`,
/// or `### Earlier` for what was there before it kept runs.
static RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^### (\d{4}-\d{2}-\d{2}|Earlier)\b").expect("valid regex"));

#[derive(Clone, Debug, Serialize)]
pub struct Plan {
    pub slug: String,
    pub title: String,
    pub task: Option<String>,
    pub created: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct SavePlan {
    pub title: Option<String>,
    #[serde(deserialize_with = "double_option")]
    pub task: Option<Option<String>>,
    pub content: String,
}

fn summarize(slug: &str, data: &Map, content: Option<String>) -> Plan {
    Plan {
        slug: slug.to_string(),
        title: text_or(data.get("title"), slug),
        task: opt_text(data.get("task")),
        created: text_or(data.get("created"), ""),
        content,
    }
}

/// Frontmatter for a plan created for a task.
pub(super) fn new_plan_data(task_title: &str, task_slug: &str) -> Map {
    let mut data = Map::new();
    data.insert("title".into(), format!("Plan: {task_title}").into());
    data.insert("task".into(), task_slug.into());
    data.insert("created".into(), dates::now_iso().into());
    data
}

/// The runs in a recurring task's summary, newest first, each with its
/// heading. Text before the first run is one run of its own.
fn runs_of(section: &str) -> Vec<String> {
    let starts: Vec<usize> = RUN.find_iter(section).map(|m| m.start()).collect();
    let mut runs = Vec::new();
    let head = section[..starts.first().copied().unwrap_or(section.len())].trim();
    if !head.is_empty() {
        runs.push(format!("### Earlier\n\n{head}"));
    }
    for (i, &start) in starts.iter().enumerate() {
        let end = starts.get(i + 1).copied().unwrap_or(section.len());
        runs.push(section[start..end].trim().to_string());
    }
    runs
}

/// A plan written over `old`: a recurring task's summary of its runs stays,
/// when the new plan doesn't have one.
pub(super) fn keep_summary(old: &str, new: &str) -> String {
    match old.find(SUMMARY_HEADING) {
        Some(i) if !new.contains(SUMMARY_HEADING) => {
            format!("{}\n\n{}", new.trim_end(), &old[i..])
        }
        _ => new.to_string(),
    }
}

/// Known keys in order, then any others the file had.
fn ordered(data: Map) -> Map {
    let mut out = Map::new();
    for key in PLAN_KEYS {
        if let Some(v) = data.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    for (k, v) in data {
        if !PLAN_KEYS.contains(&k.as_str()) {
            out.insert(k, v);
        }
    }
    out
}

impl Workspace {
    pub fn list_plans(&self) -> Result<Vec<Plan>> {
        Ok(self
            .store
            .list(Dir::Plans)?
            .iter()
            .map(|e| summarize(&e.slug, &e.doc.data, None))
            .collect())
    }

    /// A plan, or an empty placeholder if it hasn't been written yet (a task
    /// reserves its plan slug before the file exists).
    pub fn get_plan(&self, slug: &str) -> Result<Plan> {
        slug::validate(slug)?;
        Ok(match self.store.read(Dir::Plans, slug)? {
            Some(doc) => summarize(slug, &doc.data, Some(doc.content)),
            None => Plan {
                slug: slug.into(),
                title: slug.into(),
                task: None,
                created: String::new(),
                content: Some(String::new()),
            },
        })
    }

    pub fn save_plan(&self, slug: &str, req: SavePlan) -> Result<Plan> {
        slug::validate(slug)?;
        let _guard = self.writing();
        let mut data = self
            .store
            .read(Dir::Plans, slug)?
            .map(|d| d.data)
            .unwrap_or_default();
        let title = req
            .title
            .unwrap_or_else(|| text_or(data.get("title"), slug));
        data.insert("title".into(), title.into());
        let task = match req.task {
            Some(task) => task.into(),
            None => data.get("task").cloned().unwrap_or(Value::Null),
        };
        data.insert("task".into(), task);
        let created = data
            .get("created")
            .cloned()
            .filter(|v| !v.is_null())
            .unwrap_or_else(|| dates::now_iso().into());
        data.insert("created".into(), created);
        let data = ordered(data);
        self.store.write(Dir::Plans, slug, &data, &req.content)?;
        Ok(summarize(slug, &data, None))
    }

    pub fn delete_plan(&self, slug: &str) -> Result<()> {
        slug::validate(slug)?;
        let _guard = self.writing();
        if self.store.remove(Dir::Plans, slug)? {
            Ok(())
        } else {
            Err(Error::NotFound(format!("Plan not found: {slug}")))
        }
    }

    /// Put `summary` at the end of the plan as its "## Completion Summary",
    /// replacing an earlier one. A recurring task's `run` (the occurrence it
    /// was for) goes first instead, under its own heading, above the runs
    /// before it. Callers hold the write lock.
    pub(super) fn append_completion_summary(
        &self,
        plan: &str,
        task_slug: &str,
        task_title: &str,
        summary: &str,
        run: Option<&str>,
    ) -> Result<()> {
        slug::validate(plan)?;
        let (data, body) = match self.store.read(Dir::Plans, plan)? {
            Some(doc) => (doc.data, doc.content),
            None => (new_plan_data(task_title, task_slug), String::new()),
        };
        let section = match run {
            None => format!("{SUMMARY_HEADING}{summary}"),
            Some(run) => {
                let earlier = body
                    .find(SUMMARY_HEADING)
                    .map_or("", |i| &body[i + SUMMARY_HEADING.len()..]);
                let mut runs = vec![format!("### {run}\n\n{}", summary.trim())];
                runs.extend(runs_of(earlier));
                runs.truncate(MAX_RUNS);
                format!("{SUMMARY_HEADING}{}", runs.join("\n\n"))
            }
        };
        let body = match body.find(SUMMARY_HEADING) {
            Some(i) => format!("{}{section}", &body[..i]),
            None if body.is_empty() => section,
            None => format!("{body}\n\n{section}"),
        };
        self.store.write(Dir::Plans, plan, &data, &body)
    }
}
