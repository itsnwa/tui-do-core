//! Daily notes: `daily/<YYYY-MM-DD>.md`, with an optional AI one-line summary.

use serde::Serialize;

use super::store::Dir;
use super::tasks::{Task, TaskFilter};
use super::{Error, Result, Workspace, dates};
use crate::format::{Map, Value, js};

#[derive(Debug, Serialize)]
pub struct DailyNote {
    pub date: String,
    pub content: String,
    /// Tasks due that day (including recurring ones) and undated tasks
    /// completed that day.
    pub tasks: Vec<Task>,
}

#[derive(Debug, Serialize)]
pub struct DailySummary {
    pub date: String,
    pub preview: String,
    pub summary: Option<String>,
}

/// The first 120 characters (UTF-16 units, as in the app) on one line.
pub(super) fn preview(content: &str) -> String {
    let units: Vec<u16> = content.encode_utf16().take(120).collect();
    js::trim(&String::from_utf16_lossy(&units).replace('\n', " ")).to_string()
}

fn check_date(date: &str) -> Result<()> {
    if dates::is_date(date) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "Invalid date format: \"{date}\". Expected YYYY-MM-DD"
        )))
    }
}

impl Workspace {
    pub fn get_daily_note(&self, date: &str) -> Result<DailyNote> {
        check_date(date)?;
        let content = self
            .store
            .read(Dir::Daily, date)?
            .map(|d| d.content)
            .unwrap_or_default();
        let mut tasks = self.list_tasks(&TaskFilter {
            due: Some(date.into()),
            ..TaskFilter::default()
        })?;
        let completed = self.list_tasks(&TaskFilter {
            state: Some("completed".into()),
            ..TaskFilter::default()
        })?;
        tasks.extend(completed.into_iter().filter(|t| {
            t.due.is_none()
                && t.completed.as_ref().is_some_and(|c| {
                    dates::local_date_of(&Value::String(c.clone())).as_deref() == Some(date)
                })
        }));
        Ok(DailyNote {
            date: date.into(),
            content,
            tasks,
        })
    }

    pub fn list_daily_notes(&self) -> Result<Vec<DailySummary>> {
        let mut notes: Vec<DailySummary> = self
            .store
            .list(Dir::Daily)?
            .into_iter()
            .map(|e| DailySummary {
                date: dates::to_date_string(
                    e.doc
                        .data
                        .get("date")
                        .filter(|v| !v.is_null())
                        .unwrap_or(&Value::String(e.slug)),
                ),
                preview: preview(&e.doc.content),
                summary: super::tasks::opt_text(e.doc.data.get("summary")),
            })
            .collect();
        notes.sort_by(|a, b| b.date.cmp(&a.date));
        Ok(notes)
    }

    /// Save a note's text, keeping its summary. Blank text deletes the note.
    pub fn save_daily_note(&self, date: &str, content: &str) -> Result<()> {
        check_date(date)?;
        let _guard = self.writing();
        if js::trim(content).is_empty() {
            self.store.remove(Dir::Daily, date)?;
            return Ok(());
        }
        let summary = self
            .store
            .read(Dir::Daily, date)?
            .and_then(|d| d.data.get("summary").cloned())
            .unwrap_or(Value::Null);
        let mut data = Map::new();
        data.insert("date".into(), date.into());
        data.insert("summary".into(), summary);
        self.store.write(Dir::Daily, date, &data, content)
    }

    /// Store an AI summary, but only if the note still has the text it was
    /// made from. Returns whether it was stored.
    pub fn set_daily_summary(&self, date: &str, content: &str, summary: &str) -> Result<bool> {
        check_date(date)?;
        let _guard = self.writing();
        let Some(doc) = self.store.read(Dir::Daily, date)? else {
            return Ok(false);
        };
        if doc.content != js::trim(content) {
            return Ok(false);
        }
        let mut data = Map::new();
        data.insert("date".into(), date.into());
        data.insert("summary".into(), summary.into());
        self.store.write(Dir::Daily, date, &data, content)?;
        Ok(true)
    }
}
