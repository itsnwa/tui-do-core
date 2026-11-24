//! The Assistant: a Claude Code session on the home core that isn't on any
//! project, for TUI-DO itself. The user asks it to add or list tasks, start
//! agents, do work on a machine that needs no task, or set up a project.
//!
//! It runs in its own folder next to the core's (`~/.tui-do/assistant`),
//! whose `CLAUDE.md` the core writes. Its MCP server (`tui-do-core mcp`)
//! knows it's the Assistant's from `TUI_DO_SESSION`, and gives it the
//! machine and session tools the task agents don't get.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// The Assistant's session id. Task slugs never have a dot, so no task's
/// session can take it.
pub const SESSION_ID: &str = "tui-do.assistant";

/// The label every session the Assistant is part of has, and its values.
pub const ROLE_LABEL: &str = "role";
pub const ROLE: &str = "assistant";
pub const WORKER_ROLE: &str = "worker";
/// A worker session's title, as the Assistant named it.
pub const TITLE_LABEL: &str = "title";

/// Whether this process runs in the Assistant's session.
pub fn is_own_session() -> bool {
    std::env::var(crate::hooks::SESSION_ENV).ok().as_deref() == Some(SESSION_ID)
}

/// The Assistant's folder: next to the core's.
pub fn folder(core_dir: &Path) -> PathBuf {
    core_dir
        .parent()
        .map_or_else(|| core_dir.join("assistant"), |p| p.join("assistant"))
}

/// Make the folder and write its `CLAUDE.md` (the core's; the user's own
/// notes go in `CLAUDE.local.md`).
pub fn prepare(folder: &Path, home_name: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let path = folder.join("CLAUDE.md");
    let content = instructions(home_name);
    if std::fs::read_to_string(&path).ok().as_deref() != Some(content.as_str()) {
        std::fs::write(&path, content).with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(())
}

fn instructions(home_name: &str) -> String {
    format!(
        "# TUI-DO Assistant

You're the Assistant in TUI-DO: the user's agent for TUI-DO itself, not for one project. The user talks to you from the TUI-DO app on their iPhone or Mac, often by dictation, so a message can be short or loosely worded. Keep answers short: the screen is small.

You run on {home_name}, the TUI-DO home, the machine that holds the tasks. TUI-DO writes this file each time it starts you, so keep notes of your own in `CLAUDE.local.md` or your memory.

## What you do

- Tasks, projects, plans, daily notes and weekly updates, with the TUI-DO tools (`list_tasks`, `create_task`, `update_task`, …). \"What's coming up\" means open tasks due soon, by due date.
- Look in on work: `list_sessions` shows the sessions on every machine and what their agents are doing, and `read_session` shows a session's screen.
- Start a coding agent on a task with `start_agent`.
- Work that needs no task, on any machine: `start_session` starts Claude Code there with your prompt, and the user can watch it and take over in the app. Use it for anything on another machine, or longer than a few commands here.
- New projects: `set_up_project` makes the folder (a new repo or a clone, optionally on GitHub) and the TUI-DO project in one go. `list_machines` and `list_folders` help pick where it goes.

## How

- You don't work on tasks yourself: don't call `start_task`, `update_plan` or `complete_task` unless the user asks you to. To get a task done, start an agent on it.
- Ask first before deleting anything, before starting an agent or a session the user didn't ask for, and when a project's machine, folder or repo isn't clear.
- Machines can be given by id or name, and folders on them as `~/…`.
- When you start something, say which machine it's on, so the user can find it in the app.
"
    )
}

/// Whether Claude Code has a conversation in `folder` to continue.
pub fn has_conversation(folder: &Path) -> bool {
    let Some(config) = claude_config_dir() else {
        return false;
    };
    // Claude goes by the folder it's in, with links resolved
    let folder = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.to_path_buf());
    let dir = config.join("projects").join(project_key(&folder));
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|e| e.path().extension().is_some_and(|ext| ext == "jsonl"))
    })
}

fn claude_config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude"))
}

/// The name Claude Code keeps a folder's conversations under: the path with
/// everything but letters and digits as `-`.
fn project_key(folder: &Path) -> String {
    folder
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// What's typed into the Assistant's shell to start Claude.
pub fn command(continue_conversation: bool) -> String {
    let mut cmd = format!(
        "claude --settings \"${}\" -n Assistant",
        crate::hooks::SETTINGS_ENV
    );
    if continue_conversation {
        cmd += " --continue";
    }
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_session_id_should_never_be_a_task_slug() {
        let slug = crate::workspace::slug::slugify(SESSION_ID);
        assert_ne!(slug, SESSION_ID);
    }

    #[test]
    fn the_folder_should_be_next_to_the_cores() {
        assert_eq!(
            folder(Path::new("/home/me/.tui-do/core")),
            Path::new("/home/me/.tui-do/assistant")
        );
    }

    #[test]
    fn conversations_should_be_found_where_claude_keeps_them() {
        assert_eq!(
            project_key(Path::new("/home/me/.tui-do/assistant")),
            "-home-me--tui-do-assistant"
        );
    }

    #[test]
    fn the_command_should_continue_only_when_asked() {
        assert_eq!(
            command(false),
            "claude --settings \"$TUI_DO_CLAUDE_SETTINGS\" -n Assistant"
        );
        assert!(command(true).ends_with(" --continue"));
    }
}
