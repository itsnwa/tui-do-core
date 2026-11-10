//! Claude Code hook integration.
//!
//! Claude sessions are launched with `claude --settings "$TUI_DO_CLAUDE_SETTINGS"`.
//! Each hook writes a state word (idle / working / waiting) to the session's
//! state file (`$TUI_DO_CLAUDE_STATE_FILE`), which the session actor polls.
//! This reports Claude's state exactly instead of guessing from output timing.
//! The hooks that report waiting also save what Claude sent them next to the
//! state file, so a notification can say what it's waiting for.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::json;

use crate::session::AgentState;

pub const SETTINGS_ENV: &str = "TUI_DO_CLAUDE_SETTINGS";
pub const STATE_FILE_ENV: &str = "TUI_DO_CLAUDE_STATE_FILE";
/// The session's id, for what the agent runs from inside it.
pub const SESSION_ENV: &str = "TUI_DO_SESSION";
/// This core's executable, which Codex's `notify` command runs.
pub const CORE_EXE_ENV: &str = "TUI_DO_CORE_EXE";

pub struct Hooks {
    settings_path: PathBuf,
    state_dir: PathBuf,
}

fn write_state(state: &str) -> serde_json::Value {
    json!([{
        "type": "command",
        "command": format!(
            "test -n \"${STATE_FILE_ENV}\" && printf {state} > \"${STATE_FILE_ENV}\"; true"
        ),
        "timeout": 5,
    }])
}

/// Saves the hook's input before reporting waiting, so it's there when the
/// state changes.
fn write_waiting() -> serde_json::Value {
    json!([{
        "type": "command",
        "command": format!(
            "test -n \"${STATE_FILE_ENV}\" && {{ cat > \"${STATE_FILE_ENV}.json\"; printf waiting > \"${STATE_FILE_ENV}\"; }}; true"
        ),
        "timeout": 5,
    }])
}

impl Hooks {
    /// Write the hook settings file under `dir` and create the state folder.
    pub fn prepare(dir: &Path) -> anyhow::Result<Self> {
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;

        let settings = json!({
            "hooks": {
                "SessionStart": [{ "matcher": "startup|resume|clear", "hooks": write_state("idle") }],
                "UserPromptSubmit": [{ "hooks": write_state("working") }],
                "PreToolUse": [{ "matcher": "AskUserQuestion|ExitPlanMode", "hooks": write_waiting() }],
                "PostToolUse": [{ "hooks": write_state("working") }],
                "Notification": [{
                    "matcher": "permission_prompt|idle_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input|worker_permission_prompt",
                    "hooks": write_waiting(),
                }],
                "Stop": [{ "hooks": write_waiting() }],
                "SessionEnd": [{ "hooks": write_state("idle") }],
            }
        });
        let settings_path = dir.join("settings.json");
        let content = serde_json::to_string_pretty(&settings)? + "\n";
        if std::fs::read_to_string(&settings_path).ok().as_deref() != Some(content.as_str()) {
            std::fs::write(&settings_path, content)
                .with_context(|| format!("writing {}", settings_path.display()))?;
        }
        Ok(Self {
            settings_path,
            state_dir,
        })
    }

    /// Where an agent session's state is reported.
    pub fn state_file(&self, id: &str) -> PathBuf {
        self.state_dir.join(format!("{id}.state"))
    }

    /// Env for an agent session, starting from a clean state file.
    pub fn env_for(&self, id: &str, exe: &Path) -> (Vec<(String, String)>, PathBuf) {
        let state_file = self.state_file(id);
        let _ = std::fs::remove_file(&state_file);
        let _ = std::fs::remove_file(detail_file(&state_file));
        let env = vec![
            (
                SETTINGS_ENV.to_string(),
                self.settings_path.display().to_string(),
            ),
            (STATE_FILE_ENV.to_string(), state_file.display().to_string()),
            (SESSION_ENV.to_string(), id.to_string()),
            (CORE_EXE_ENV.to_string(), exe.display().to_string()),
        ];
        (env, state_file)
    }
}

/// Whether sessions of this agent report their state to a state file.
pub fn reports_state(agent: Option<&str>) -> bool {
    matches!(agent, Some("claude" | "codex"))
}

pub fn read_state(path: &Path) -> Option<AgentState> {
    match std::fs::read_to_string(path).ok()?.trim() {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "waiting" => Some(AgentState::Waiting),
        _ => None,
    }
}

/// Where the hook that reported waiting left its input (Codex: its notify payload).
pub fn detail_file(state_file: &Path) -> PathBuf {
    let mut path = state_file.as_os_str().to_owned();
    path.push(".json");
    path.into()
}

/// The longest detail a notification gets.
const DETAIL_MAX: usize = 200;

/// What the agent is waiting for, in one line: a question it asked, a
/// permission it needs, or the last line of its reply.
pub fn read_detail(state_file: &Path) -> Option<String> {
    let text = std::fs::read_to_string(detail_file(state_file)).ok()?;
    detail(&serde_json::from_str(&text).ok()?)
}

fn detail(input: &serde_json::Value) -> Option<String> {
    let text = if let Some(question) = input["tool_input"]["questions"][0]["question"].as_str() {
        question.to_string()
    } else if input["tool_name"] == "ExitPlanMode" {
        "Plan ready for review".to_string()
    } else if let Some(message) = input["message"].as_str() {
        message.to_string()
    } else {
        let reply = input["last_assistant_message"]
            .as_str()
            .or_else(|| input["last-assistant-message"].as_str())?;
        reply
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())?
            .to_string()
    };
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match line.char_indices().nth(DETAIL_MAX) {
        None if line.is_empty() => None,
        None => Some(line),
        Some((end, _)) => Some(format!("{}…", line[..end].trim_end())),
    }
}

#[cfg(test)]
mod tests {
    use super::detail;
    use serde_json::json;

    #[test]
    fn detail_should_say_what_the_agent_waits_for() {
        let asked = json!({ "tool_name": "AskUserQuestion", "tool_input": { "questions": [{ "question": "Which one?" }] } });
        assert_eq!(detail(&asked).as_deref(), Some("Which one?"));
        let plan = json!({ "tool_name": "ExitPlanMode", "tool_input": { "plan": "# Plan" } });
        assert_eq!(detail(&plan).as_deref(), Some("Plan ready for review"));
        let permission = json!({ "message": "Claude needs your permission to use Bash" });
        assert_eq!(
            detail(&permission).as_deref(),
            Some("Claude needs your permission to use Bash")
        );
        let stop = json!({ "last_assistant_message": "Done.\n\nShould I  push it?\n\n" });
        assert_eq!(detail(&stop).as_deref(), Some("Should I push it?"));
        let codex = json!({ "last-assistant-message": "All set" });
        assert_eq!(detail(&codex).as_deref(), Some("All set"));
        assert_eq!(detail(&json!({ "last_assistant_message": " \n" })), None);
        assert_eq!(detail(&json!({})), None);
    }

    #[test]
    fn long_detail_should_be_cut() {
        let long = json!({ "message": "é".repeat(300) });
        let cut = detail(&long).expect("detail");
        assert_eq!(cut.chars().count(), 201);
        assert!(cut.ends_with('…'));
    }
}
