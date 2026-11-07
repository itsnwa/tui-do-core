//! Links from a task to the Claude Code session working on it, so a client
//! can resume that session. Session IDs are local to a machine, so links live
//! in the core's own folder (`task-sessions/<task-slug>.json`), not in the
//! shared workspace.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionLink {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub linked_at: Option<String>,
}

impl SessionLink {
    /// A link is only stored for a real session: a UUID and an absolute directory.
    pub fn validate(&self) -> Result<(), String> {
        if uuid::Uuid::parse_str(&self.session_id).is_err() {
            return Err(format!(
                "sessionId must be a UUID, got {:?}",
                self.session_id
            ));
        }
        if !Path::new(&self.cwd).is_absolute() {
            return Err(format!("cwd must be an absolute path, got {:?}", self.cwd));
        }
        Ok(())
    }

    /// The Claude Code session that launched this process, from
    /// `~/.claude/sessions/<parent pid>.json`. Only set when Claude Code is
    /// the parent (`CLAUDECODE=1`).
    pub fn of_parent_claude() -> Option<Self> {
        if std::env::var("CLAUDECODE").ok().as_deref() != Some("1") {
            return None;
        }
        let ppid = std::os::unix::process::parent_id();
        let home = std::env::var_os("HOME")?;
        let path = PathBuf::from(home)
            .join(".claude/sessions")
            .join(format!("{ppid}.json"));
        let json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
        if json.get("pid").and_then(serde_json::Value::as_u64) != Some(u64::from(ppid)) {
            return None;
        }
        let link = SessionLink {
            session_id: json.get("sessionId")?.as_str()?.to_string(),
            cwd: json.get("cwd")?.as_str()?.to_string(),
            linked_at: None,
        };
        link.validate().ok().map(|()| link)
    }
}

pub struct TaskSessions {
    dir: PathBuf,
}

impl TaskSessions {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    fn path(&self, task_slug: &str) -> Option<PathBuf> {
        let safe = !task_slug.is_empty()
            && !task_slug.starts_with('.')
            && !task_slug.contains(['/', '\\', '\0']);
        safe.then(|| self.dir.join(format!("{task_slug}.json")))
    }

    pub fn get(&self, task_slug: &str) -> Option<SessionLink> {
        let text = std::fs::read_to_string(self.path(task_slug)?).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Store a link (atomically), stamping when it was made.
    pub fn set(&self, task_slug: &str, link: &SessionLink) -> anyhow::Result<SessionLink> {
        link.validate().map_err(anyhow::Error::msg)?;
        let path = self.path(task_slug).context("invalid task slug")?;
        std::fs::create_dir_all(&self.dir)?;
        let stored = SessionLink {
            linked_at: Some(crate::workspace::dates::now_iso()),
            ..link.clone()
        };
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        std::fs::write(&tmp, serde_json::to_string_pretty(&stored)? + "\n")?;
        std::fs::rename(&tmp, &path)?;
        Ok(stored)
    }
}
