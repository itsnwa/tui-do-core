//! Claude Code hook integration.
//!
//! Claude sessions are launched with `claude --settings "$TUI_DO_CLAUDE_SETTINGS"`.
//! Every hook in that file runs `tui-do-core hook`, which appends what Claude
//! handed it to the session's log (`$TUI_DO_CLAUDE_STATE_FILE.events`) as one
//! line, a [`Record`]. The session actor reads the log and works the
//! session's status out of it (see `status`): exactly, not guessed from
//! output timing, and again from the start after the core restarts.
//!
//! Codex has hooks of the same shape. It reads them from its own
//! `hooks.json`, where [`install_codex`] puts the same command, and only
//! runs one the user has reviewed (`/hooks` in Codex).
//!
//! Sessions whose Claude started under a core before 0.3.18 still have its
//! hooks, which write a state word (idle / working / waiting) to the state
//! file and save their input next to it. Codex's `notify` command does the
//! same when a turn ends, for a Codex whose hook isn't trusted yet. Both
//! are still read.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Context;
use serde_json::json;

use crate::session::AgentState;
use crate::status::Record;

pub const SETTINGS_ENV: &str = "TUI_DO_CLAUDE_SETTINGS";
pub const STATE_FILE_ENV: &str = "TUI_DO_CLAUDE_STATE_FILE";
/// The session's id, for what the agent runs from inside it.
pub const SESSION_ENV: &str = "TUI_DO_SESSION";
/// This core's executable, which Codex's `notify` command runs.
pub const CORE_EXE_ENV: &str = "TUI_DO_CORE_EXE";
/// The process id of the session's holder, set by the holder itself.
pub const HOLDER_ENV: &str = "TUI_DO_SESSION_HOLDER";

/// The hooks Claude reports with. `PreToolUse` and `PostToolUse` say which
/// tool calls are under way, so one finishing can't hide another's open
/// permission prompt; `PermissionRequest` fires as a prompt opens (the
/// `permission_prompt` notification only six seconds later); `PostToolBatch`
/// says every call of a batch is done, also one whose prompt was refused.
const EVENTS: [&str; 14] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PermissionDenied",
    "PostToolUse",
    "PostToolUseFailure",
    "PostToolBatch",
    "Elicitation",
    "ElicitationResult",
    "Notification",
    "Stop",
    "StopFailure",
    "SessionEnd",
];

/// The hooks Codex reports with. It has one for an interrupted turn, which
/// Claude lacks; `PreModelRequest` is its sign that a batch of tool calls is
/// done.
const CODEX_EVENTS: [&str; 9] = [
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "PermissionRequest",
    "PostToolUse",
    "PreModelRequest",
    "Stop",
    "Interrupt",
    "SessionEnd",
];

pub struct Hooks {
    settings_path: PathBuf,
    state_dir: PathBuf,
}

/// What every hook runs. The path is written out: a shell opened by an
/// earlier core doesn't have it in its environment. Outside a TUI-DO
/// session, nothing runs.
fn command(exe: &Path) -> String {
    format!(
        "test -n \"${STATE_FILE_ENV}\" && {} hook; true",
        shell_quote(&exe.display().to_string())
    )
}

/// Whether a hook's command is one of TUI-DO's, from this core or an earlier one.
fn is_ours(hook: &serde_json::Value) -> bool {
    let command = hook["command"].as_str().unwrap_or_default();
    command.contains(STATE_FILE_ENV) && command.ends_with(" hook; true")
}

/// Put TUI-DO's hook into Codex's `hooks.json`, next to whatever else is
/// there, for every event Codex reports with. True if the file changed:
/// Codex then asks the user to review the hook before it runs it.
pub fn install_codex(hooks_json: &Path, exe: &Path) -> anyhow::Result<bool> {
    let current = std::fs::read_to_string(hooks_json).ok();
    let mut json: serde_json::Value = match &current {
        Some(text) => serde_json::from_str(text)
            .with_context(|| format!("{} isn't valid JSON", hooks_json.display()))?,
        None => json!({}),
    };
    let root = json
        .as_object_mut()
        .context("Codex's hooks.json isn't a JSON object")?;
    let events = root
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("hooks isn't an object")?;
    for event in CODEX_EVENTS {
        // Codex gives these two three seconds at most, and says so if asked for more
        let timeout = if matches!(event, "SessionEnd" | "Interrupt") {
            3
        } else {
            5
        };
        let ours = json!({ "type": "command", "command": command(exe), "timeout": timeout });
        let groups = events
            .entry(event)
            .or_insert_with(|| json!([]))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} isn't a list"))?;
        // An earlier core's, at another path: this one takes its place
        for group in groups.iter_mut() {
            if let Some(hooks) = group["hooks"].as_array_mut() {
                hooks.retain(|hook| !is_ours(hook));
            }
        }
        groups.retain(|group| {
            group["hooks"]
                .as_array()
                .is_none_or(|hooks| !hooks.is_empty())
        });
        groups.push(json!({ "hooks": [ours] }));
    }
    let updated = serde_json::to_string_pretty(&json)? + "\n";
    // The same hooks, however the file was laid out
    let same = current
        .as_deref()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(text).ok())
        .is_some_and(|before| before == json);
    if same {
        return Ok(false);
    }
    if let Some(dir) = hooks_json.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(hooks_json, updated)
        .with_context(|| format!("writing {}", hooks_json.display()))?;
    Ok(true)
}

/// Where Codex keeps its own files: `$CODEX_HOME`, else `~/.codex`.
pub fn codex_home() -> Option<PathBuf> {
    match std::env::var_os("CODEX_HOME").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".codex")),
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', r"'\''"))
}

impl Hooks {
    /// Write the hook settings file under `dir` and create the state folder.
    /// `exe` is this core's binary, which the hooks run.
    pub fn prepare(dir: &Path, exe: &Path) -> anyhow::Result<Self> {
        let state_dir = dir.join("state");
        std::fs::create_dir_all(&state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;

        let command = command(exe);
        let hooks: serde_json::Map<String, serde_json::Value> = EVENTS
            .iter()
            .map(|event| {
                let run =
                    json!([{ "hooks": [{ "type": "command", "command": command, "timeout": 5 }] }]);
                ((*event).to_string(), run)
            })
            .collect();
        let settings = json!({ "hooks": hooks });
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
        remove_state(&state_file);
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

/// Forget what a session's agent reported.
pub fn remove_state(state_file: &Path) {
    let _ = std::fs::remove_file(state_file);
    let _ = std::fs::remove_file(detail_file(state_file));
    let _ = std::fs::remove_file(log_file(state_file));
}

/// Where a session's hooks log their events.
pub fn log_file(state_file: &Path) -> PathBuf {
    let mut path = state_file.as_os_str().to_owned();
    path.push(".events");
    path.into()
}

/// `tui-do-core hook`: what every hook of a TUI-DO session runs, with the
/// agent's hook input on stdin. Adds its record to the session's log.
///
/// It prints nothing and never fails: a hook's output and exit code can
/// change what the agent does, and reporting must not.
pub fn run() {
    let Some(state_file) = std::env::var_os(STATE_FILE_ENV).filter(|path| !path.is_empty()) else {
        return;
    };
    if !in_its_session() {
        return;
    }
    let mut input = String::new();
    if std::io::stdin().read_to_string(&mut input).is_err() {
        return;
    }
    let Ok(input) = serde_json::from_str::<serde_json::Value>(&input) else {
        return;
    };
    let now = crate::workspace::dates::now_millis();
    if let Some(record) = Record::of_hook(&input, now) {
        let _ = append(&log_file(Path::new(&state_file)), &record);
    }
}

/// Whether this process runs in the session its environment names. An agent
/// can hand its sessions to a background server that outlives them (Codex
/// does): started from inside a TUI-DO session, the server keeps that
/// session's environment, and the hooks of every session it runs afterwards
/// would report for it. Those don't descend from the session's holder.
fn in_its_session() -> bool {
    let holder = std::env::var(HOLDER_ENV)
        .ok()
        .and_then(|pid| pid.parse().ok());
    match holder {
        Some(holder) => descends_from(holder),
        // A session started by a core before 0.3.23 doesn't say
        None => true,
    }
}

/// Whether `ancestor` is this process or one it descends from. True also
/// when the system won't say whose child a process is: better a report too
/// many than none.
fn descends_from(ancestor: u32) -> bool {
    let mut pid = std::process::id();
    // Far deeper than any process tree; ends a loop if the system reports one
    for _ in 0..64 {
        if pid == ancestor {
            return true;
        }
        match parent_of(pid) {
            Some(parent) if parent > 1 && parent != pid => pid = parent,
            // The top: a process whose parent is gone belongs to init
            Some(_) => return false,
            None => return true,
        }
    }
    false
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (name) state ppid …`; the name can have spaces and brackets itself
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(target_os = "macos")]
fn parent_of(pid: u32) -> Option<u32> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
    // Fills `info` with what the kernel knows about the process, or fails
    let filled = unsafe {
        libc::proc_pidinfo(
            libc::c_int::try_from(pid).ok()?,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            size,
        )
    };
    (filled == size).then(|| unsafe { info.assume_init() }.pbi_ppid)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn parent_of(_pid: u32) -> Option<u32> {
    None
}

/// Add a record to a log as one line, in one write, so hooks that run at the
/// same time (parallel tool calls) can't tear each other's lines.
pub fn append(log: &Path, record: &Record) -> anyhow::Result<()> {
    let mut line = serde_json::to_string(record)?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    file.write_all(line.as_bytes())?;
    Ok(())
}

/// A session's log, read as it grows.
pub struct Log {
    path: PathBuf,
    /// How far it's been read.
    position: u64,
    /// The start of a line whose end hasn't been written yet.
    partial: Vec<u8>,
}

impl Log {
    pub fn new(state_file: &Path) -> Self {
        Self {
            path: log_file(state_file),
            position: 0,
            partial: Vec::new(),
        }
    }

    /// The records added since the last call.
    pub fn read_new(&mut self) -> Vec<Record> {
        let Ok(size) = std::fs::metadata(&self.path).map(|meta| meta.len()) else {
            return Vec::new();
        };
        if size < self.position {
            // A new log under the same name
            self.position = 0;
            self.partial.clear();
        }
        if size == self.position {
            return Vec::new();
        }
        let mut added = Vec::new();
        let read = std::fs::File::open(&self.path).and_then(|mut file| {
            file.seek(SeekFrom::Start(self.position))?;
            file.read_to_end(&mut added)
        });
        let Ok(read) = read else {
            return Vec::new();
        };
        self.position += read as u64;
        self.partial.extend_from_slice(&added);
        let Some(end) = self.partial.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let lines: Vec<u8> = self.partial.drain(..=end).collect();
        String::from_utf8_lossy(&lines)
            .lines()
            .filter_map(|line| serde_json::from_str(line).ok())
            .collect()
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

/// Where a hook that wrote the state word `waiting` left its input (Codex:
/// its notify payload).
pub fn detail_file(state_file: &Path) -> PathBuf {
    let mut path = state_file.as_os_str().to_owned();
    path.push(".json");
    path.into()
}

/// The longest detail a notification gets.
const DETAIL_MAX: usize = 200;

/// What the agent is waiting for, in one line: a question it asked, a
/// permission it needs, the error its turn ended on, or the last line of its
/// reply.
pub fn read_detail(state_file: &Path) -> Option<String> {
    detail(&read_input(state_file)?)
}

/// The input of the hook that wrote the state word `waiting`.
pub fn read_input(state_file: &Path) -> Option<serde_json::Value> {
    let text = std::fs::read_to_string(detail_file(state_file)).ok()?;
    serde_json::from_str(&text).ok()
}

/// One line on what a hook's input says the agent waits for.
pub(crate) fn detail(input: &serde_json::Value) -> Option<String> {
    let text = if let Some(question) = input["tool_input"]["questions"][0]["question"].as_str() {
        question.to_string()
    } else if input["tool_name"] == "ExitPlanMode" {
        "Plan ready for review".to_string()
    } else if input["hook_event_name"] == "PermissionRequest" {
        permission(input)
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

/// The permission Claude asks for: the tool, and what it would do with it.
fn permission(input: &serde_json::Value) -> String {
    let tool = input["tool_name"].as_str().unwrap_or("a tool");
    let what = ["command", "file_path", "notebook_path", "url", "query"]
        .iter()
        .find_map(|key| input["tool_input"][key].as_str());
    match what {
        Some(what) => format!("Allow {tool}: {what}?"),
        None => format!("Allow {tool}?"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_hook_should_run_the_core_with_its_path_quoted() {
        let dir = std::env::temp_dir().join(format!("tui-do-hooks-{}", uuid::Uuid::new_v4()));
        let hooks = Hooks::prepare(&dir, Path::new("/Users/it's me/tui-do-core")).expect("hooks");
        let text = std::fs::read_to_string(&hooks.settings_path).expect("settings");
        let settings: serde_json::Value = serde_json::from_str(&text).expect("json");
        let events = settings["hooks"].as_object().expect("hooks");
        assert_eq!(events.len(), EVENTS.len());
        for (event, groups) in events {
            assert_eq!(
                groups[0]["hooks"][0]["command"],
                r#"test -n "$TUI_DO_CLAUDE_STATE_FILE" && '/Users/it'\''s me/tui-do-core' hook; true"#,
                "{event}"
            );
        }
        std::fs::remove_dir_all(dir).expect("clean up");
    }

    #[test]
    fn codex_s_hooks_should_gain_ours_and_keep_the_user_s() {
        let dir = std::env::temp_dir().join(format!("tui-do-codex-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("folder");
        let file = dir.join("hooks.json");
        let theirs = json!({ "hooks": { "SessionStart": [{ "hooks": [
            { "type": "command", "command": "bash their-hook.sh", "timeout": 10 },
        ] }] } });
        std::fs::write(&file, theirs.to_string()).expect("write");

        assert!(install_codex(&file, Path::new("/old/tui-do-core")).expect("install"));
        // Nothing to review again when nothing changed
        assert!(!install_codex(&file, Path::new("/old/tui-do-core")).expect("install"));
        // The core moved: its hook is replaced, not added to
        assert!(install_codex(&file, Path::new("/new/tui-do-core")).expect("install"));

        let text = std::fs::read_to_string(&file).expect("read");
        let json: serde_json::Value = serde_json::from_str(&text).expect("json");
        let events = json["hooks"].as_object().expect("hooks");
        assert_eq!(events.len(), CODEX_EVENTS.len());
        let commands = |event: &str| -> Vec<String> {
            let groups = events[event].as_array().expect("groups");
            let hooks = groups
                .iter()
                .flat_map(|group| group["hooks"].as_array().expect("hooks"));
            hooks
                .map(|hook| hook["command"].as_str().expect("command").to_string())
                .collect()
        };
        let ours = r#"test -n "$TUI_DO_CLAUDE_STATE_FILE" && '/new/tui-do-core' hook; true"#;
        assert_eq!(commands("SessionStart"), ["bash their-hook.sh", ours]);
        assert_eq!(commands("Interrupt"), [ours]);
        assert_eq!(events["Interrupt"][0]["hooks"][0]["timeout"], 3);
        assert_eq!(events["Stop"][0]["hooks"][0]["timeout"], 5);
        assert!(!text.contains("/old/"));
        std::fs::remove_dir_all(dir).expect("clean up");
    }

    #[test]
    fn codex_s_hooks_should_be_left_alone_when_they_can_t_be_read() {
        let dir = std::env::temp_dir().join(format!("tui-do-codex-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("folder");
        let file = dir.join("hooks.json");
        std::fs::write(&file, "{ not json").expect("write");
        assert!(install_codex(&file, Path::new("/tui-do-core")).is_err());
        assert_eq!(std::fs::read_to_string(&file).expect("read"), "{ not json");
        std::fs::remove_dir_all(dir).expect("clean up");
    }

    #[test]
    fn a_process_should_descend_from_its_parent_and_not_from_a_stranger() {
        let parent = std::os::unix::process::parent_id();
        assert_eq!(parent_of(std::process::id()), Some(parent));
        assert!(descends_from(std::process::id()));
        assert!(descends_from(parent));
        // Far above any pid the system hands out
        assert!(!descends_from(0x3fff_fff0));
    }

    #[test]
    fn a_log_should_give_each_record_once_as_it_grows() {
        let dir = std::env::temp_dir().join(format!("tui-do-log-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("folder");
        let state_file = dir.join("task.state");
        let record = |event: &str| Record {
            t: 1,
            event: event.into(),
            ..Record::default()
        };
        let mut log = Log::new(&state_file);
        assert!(log.read_new().is_empty());

        append(&log_file(&state_file), &record("SessionStart")).expect("append");
        append(&log_file(&state_file), &record("UserPromptSubmit")).expect("append");
        let events =
            |records: Vec<Record>| records.into_iter().map(|r| r.event).collect::<Vec<_>>();
        assert_eq!(events(log.read_new()), ["SessionStart", "UserPromptSubmit"]);
        assert!(log.read_new().is_empty());

        // Half a line, a line that isn't a record, then the rest
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(log_file(&state_file))
            .expect("open");
        file.write_all(b"junk\n{\"t\":2,\"event\":\"St")
            .expect("write");
        assert!(log.read_new().is_empty());
        file.write_all(b"op\"}\n").expect("write");
        assert_eq!(events(log.read_new()), ["Stop"]);

        // The session started over
        remove_state(&state_file);
        append(&log_file(&state_file), &record("SessionStart")).expect("append");
        assert_eq!(events(log.read_new()), ["SessionStart"]);
        std::fs::remove_dir_all(dir).expect("clean up");
    }

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
        let failed = json!({ "hook_event_name": "StopFailure", "error": "rate_limit", "last_assistant_message": "API Error: Rate limit reached" });
        assert_eq!(
            detail(&failed).as_deref(),
            Some("API Error: Rate limit reached")
        );
        let codex = json!({ "last-assistant-message": "All set" });
        assert_eq!(detail(&codex).as_deref(), Some("All set"));
        assert_eq!(detail(&json!({ "last_assistant_message": " \n" })), None);
        assert_eq!(detail(&json!({})), None);
    }

    #[test]
    fn detail_should_say_which_permission_is_asked_for() {
        let run = json!({ "hook_event_name": "PermissionRequest", "tool_name": "Bash", "tool_input": { "command": "cargo  test\n--locked" } });
        assert_eq!(
            detail(&run).as_deref(),
            Some("Allow Bash: cargo test --locked?")
        );
        let edit = json!({ "hook_event_name": "PermissionRequest", "tool_name": "Edit", "tool_input": { "file_path": "/code/src/main.rs", "old_string": "a" } });
        assert_eq!(
            detail(&edit).as_deref(),
            Some("Allow Edit: /code/src/main.rs?")
        );
        let tool = json!({ "hook_event_name": "PermissionRequest", "tool_name": "mcp__tui-do__delete_task", "tool_input": { "task_id": "1" } });
        assert_eq!(
            detail(&tool).as_deref(),
            Some("Allow mcp__tui-do__delete_task?")
        );
        // A question or a plan is still said as one, however Claude asks
        let asked = json!({ "hook_event_name": "PermissionRequest", "tool_name": "AskUserQuestion", "tool_input": { "questions": [{ "question": "Which one?" }] } });
        assert_eq!(detail(&asked).as_deref(), Some("Which one?"));
    }

    #[test]
    fn long_detail_should_be_cut() {
        let long = json!({ "message": "é".repeat(300) });
        let cut = detail(&long).expect("detail");
        assert_eq!(cut.chars().count(), 201);
        assert!(cut.ends_with('…'));
    }
}
