//! Links from a task to the Claude Code session working on it, so a client
//! can resume that session. Session IDs are local to a machine, so links live
//! in the core's own folder (`task-sessions/<task-slug>.json`), not in the
//! shared workspace.

use std::io::{BufRead, BufReader};
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

    /// The Codex session a `notify` payload reports a finished turn for.
    pub fn of_codex_turn(payload: &str) -> Option<Self> {
        let json: serde_json::Value = serde_json::from_str(payload).ok()?;
        if json.get("type")?.as_str()? != "agent-turn-complete" {
            return None;
        }
        let link = SessionLink {
            session_id: json.get("thread-id")?.as_str()?.to_string(),
            cwd: json.get("cwd")?.as_str()?.to_string(),
            linked_at: None,
        };
        link.validate().ok().map(|()| link)
    }
}

/// The newest file under `dir` whose name ends in `suffix`, `depth` folders down.
fn find_newest(dir: &Path, suffix: &str, depth: usize) -> Option<PathBuf> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort_unstable_by(|a, b| b.cmp(a));
    entries.into_iter().find_map(|path| {
        if depth == 0 {
            path.to_string_lossy().ends_with(suffix).then_some(path)
        } else {
            find_newest(&path, suffix, depth - 1)
        }
    })
}

/// Whether a thread that reported a turn is a conversation that can be resumed,
/// from what Codex saved in `sessions/YYYY/MM/DD/rollout-…-<id>.jsonl`. Its side
/// threads report turns too: the one that titles a conversation isn't saved,
/// and a subagent's source is an object instead of the client's name.
pub fn is_codex_conversation(codex_home: &Path, session_id: &str) -> bool {
    let suffix = format!("-{session_id}.jsonl");
    let Some(path) = find_newest(&codex_home.join("sessions"), &suffix, 3) else {
        return false;
    };
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let mut first = String::new();
    if BufReader::new(file).read_line(&mut first).is_err() {
        return false;
    }
    serde_json::from_str::<serde_json::Value>(&first)
        .is_ok_and(|meta| meta["payload"]["source"].is_string())
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

    /// Every link, by task, to hand to a new home.
    pub fn all(&self) -> Vec<(String, SessionLink)> {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter_map(|e| {
                let task = e.file_name().to_str()?.strip_suffix(".json")?.to_string();
                Some((task.clone(), self.get(&task)?))
            })
            .collect()
    }

    /// Replace every link (taking over as home, or giving the role away).
    pub fn replace(&self, links: &[(String, SessionLink)]) -> anyhow::Result<()> {
        if self.dir.exists() {
            std::fs::remove_dir_all(&self.dir)?;
        }
        std::fs::create_dir_all(&self.dir)?;
        for (task, link) in links {
            let path = self.path(task).context("invalid task slug")?;
            std::fs::write(path, serde_json::to_string_pretty(link)? + "\n")?;
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    const THREAD: &str = "01a1c173-979a-70e0-bbe0-b83e52032d3f";

    /// A Codex home with a saved session, whose first line has `source`.
    fn codex_home(source: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("tui-do-links-{}", uuid::Uuid::new_v4()));
        let day = home.join("sessions/2026/11/09");
        std::fs::create_dir_all(&day).expect("folders");
        std::fs::write(
            day.join(format!("rollout-2026-11-09T19-00-00-{THREAD}.jsonl")),
            format!("{{\"type\":\"session_meta\",\"payload\":{{\"source\":{source}}}}}\nmore\n"),
        )
        .expect("rollout");
        home
    }

    #[test]
    fn a_saved_codex_session_should_be_a_conversation() {
        let home = codex_home(r#""cli""#);
        let found = is_codex_conversation(&home, THREAD);
        let _ = std::fs::remove_dir_all(&home);
        assert!(found);
    }

    #[test]
    fn a_thread_codex_did_not_save_should_not_be_a_conversation() {
        let home = codex_home(r#""cli""#);
        let found = is_codex_conversation(&home, "01a1c189-b5d4-7810-9d32-2d6f5da4275c");
        let _ = std::fs::remove_dir_all(&home);
        assert!(!found);
    }

    #[test]
    fn a_subagent_should_not_be_a_conversation() {
        let home = codex_home(r#"{"subagent":{"other":"guardian"}}"#);
        let found = is_codex_conversation(&home, THREAD);
        let _ = std::fs::remove_dir_all(&home);
        assert!(!found);
    }

    #[test]
    fn a_finished_codex_turn_should_give_its_session() {
        let payload =
            format!(r#"{{"type":"agent-turn-complete","thread-id":"{THREAD}","cwd":"/code/x"}}"#);
        let link = SessionLink::of_codex_turn(&payload).expect("a link");
        assert_eq!(
            (link.session_id.as_str(), link.cwd.as_str()),
            (THREAD, "/code/x")
        );
    }

    #[test]
    fn other_codex_notifications_should_give_no_session() {
        let payload = format!(r#"{{"type":"other","thread-id":"{THREAD}","cwd":"/code/x"}}"#);
        assert!(SessionLink::of_codex_turn(&payload).is_none());
    }

    #[test]
    fn a_codex_turn_without_a_real_session_id_should_give_no_session() {
        let payload = r#"{"type":"agent-turn-complete","thread-id":"$(x)","cwd":"/code/x"}"#;
        assert!(SessionLink::of_codex_turn(payload).is_none());
    }
}
