//! Weekly updates: `weekly/<YYYY-Www>.md`. The text is usually AI-generated
//! by a client and saved here with `generated` set.

use serde::Serialize;

use super::daily::preview;
use super::store::Dir;
use super::tasks::{opt_text, text_or};
use super::{Error, Result, Workspace, dates};
use crate::format::{Map, Value, js};

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklyUpdate {
    pub week_id: String,
    pub generated: Option<String>,
    pub content: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WeeklySummary {
    pub week_id: String,
    pub generated: Option<String>,
    pub preview: String,
    pub summary: Option<String>,
}

fn check_week(week_id: &str) -> Result<()> {
    if dates::is_week_id(week_id) {
        Ok(())
    } else {
        Err(Error::Invalid(format!(
            "Invalid week ID format: \"{week_id}\". Expected YYYY-WXX (e.g., 2026-W08)"
        )))
    }
}

fn weekly_data(week_id: &str, generated: Value, summary: Value) -> Map {
    let mut data = Map::new();
    data.insert("week".into(), week_id.into());
    data.insert("generated".into(), generated);
    data.insert("summary".into(), summary);
    data
}

impl Workspace {
    pub fn get_weekly_update(&self, week_id: &str) -> Result<WeeklyUpdate> {
        check_week(week_id)?;
        Ok(match self.store.read(Dir::Weekly, week_id)? {
            Some(doc) => WeeklyUpdate {
                week_id: week_id.into(),
                generated: opt_text(doc.data.get("generated")),
                content: doc.content,
            },
            None => WeeklyUpdate {
                week_id: week_id.into(),
                generated: None,
                content: String::new(),
            },
        })
    }

    pub fn list_weekly_updates(&self) -> Result<Vec<WeeklySummary>> {
        let mut list: Vec<WeeklySummary> = self
            .store
            .list(Dir::Weekly)?
            .into_iter()
            .map(|e| WeeklySummary {
                week_id: text_or(e.doc.data.get("week"), &e.slug),
                generated: opt_text(e.doc.data.get("generated")),
                preview: preview(&e.doc.content),
                summary: opt_text(e.doc.data.get("summary")),
            })
            .collect();
        list.sort_by(|a, b| b.week_id.cmp(&a.week_id));
        Ok(list)
    }

    /// Save an update's text. With `generated`, it's stamped as freshly
    /// generated (and its old summary dropped); otherwise both are kept.
    /// Blank text deletes the update.
    pub fn save_weekly_update(
        &self,
        week_id: &str,
        content: &str,
        generated: bool,
    ) -> Result<Option<String>> {
        check_week(week_id)?;
        let _guard = self.writing();
        if js::trim(content).is_empty() {
            self.store.remove(Dir::Weekly, week_id)?;
            return Ok(None);
        }
        let existing = self
            .store
            .read(Dir::Weekly, week_id)?
            .map(|d| d.data)
            .unwrap_or_default();
        let field = |k: &str| existing.get(k).cloned().unwrap_or(Value::Null);
        let data = if generated {
            weekly_data(week_id, dates::now_iso().into(), Value::Null)
        } else {
            weekly_data(week_id, field("generated"), field("summary"))
        };
        self.store.write(Dir::Weekly, week_id, &data, content)?;
        Ok(opt_text(data.get("generated")))
    }

    /// Store an AI summary if the update still has the text it was made from.
    pub fn set_weekly_summary(&self, week_id: &str, content: &str, summary: &str) -> Result<bool> {
        check_week(week_id)?;
        let _guard = self.writing();
        let Some(doc) = self.store.read(Dir::Weekly, week_id)? else {
            return Ok(false);
        };
        if doc.content != js::trim(content) {
            return Ok(false);
        }
        let generated = doc.data.get("generated").cloned().unwrap_or(Value::Null);
        self.store.write(
            Dir::Weekly,
            week_id,
            &weekly_data(week_id, generated, summary.into()),
            content,
        )?;
        Ok(true)
    }
}
