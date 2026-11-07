//! Claude Code hook integration.
//!
//! Claude sessions are launched with `claude --settings "$TUI_DO_CLAUDE_SETTINGS"`.
//! Each hook writes a state word (idle / working / waiting) to the session's
//! state file (`$TUI_DO_CLAUDE_STATE_FILE`), which the session actor polls.
//! This reports Claude's state exactly instead of guessing from output timing.

use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::json;

use crate::session::AgentState;

pub const SETTINGS_ENV: &str = "TUI_DO_CLAUDE_SETTINGS";
pub const STATE_FILE_ENV: &str = "TUI_DO_CLAUDE_STATE_FILE";

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
                "PreToolUse": [{ "matcher": "AskUserQuestion|ExitPlanMode", "hooks": write_state("waiting") }],
                "PostToolUse": [{ "hooks": write_state("working") }],
                "Notification": [{
                    "matcher": "permission_prompt|idle_prompt|elicitation_dialog|elicitation_url_dialog|agent_needs_input|worker_permission_prompt",
                    "hooks": write_state("waiting"),
                }],
                "Stop": [{ "hooks": write_state("waiting") }],
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

    /// Env for a Claude session, starting from a clean state file.
    pub fn env_for(&self, id: &str) -> (Vec<(String, String)>, PathBuf) {
        let state_file = self.state_dir.join(format!("{id}.state"));
        let _ = std::fs::remove_file(&state_file);
        let env = vec![
            (
                SETTINGS_ENV.to_string(),
                self.settings_path.display().to_string(),
            ),
            (STATE_FILE_ENV.to_string(), state_file.display().to_string()),
        ];
        (env, state_file)
    }
}

pub fn read_state(path: &Path) -> Option<AgentState> {
    match std::fs::read_to_string(path).ok()?.trim() {
        "idle" => Some(AgentState::Idle),
        "working" => Some(AgentState::Working),
        "waiting" => Some(AgentState::Waiting),
        _ => None,
    }
}
