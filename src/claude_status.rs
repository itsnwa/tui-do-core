//! Claude Code's own account of each running session.
//!
//! Claude keeps a file per process, `~/.claude/sessions/<pid>.json`, with
//! the session it has open and whether it's `busy`, `idle` or `waiting` for
//! the user. Hooks report a session's status first (see `status`), but two
//! things have no hook: a turn the user interrupts, and a permission prompt
//! being answered. This file knows both.
//!
//! The file isn't documented, so nothing depends on it: a session whose file
//! can't be found or read goes by its hooks alone.

use std::path::{Path, PathBuf};

use crate::status::{ClaudeStatus, Says};

/// Claude's own folder: `$CLAUDE_CONFIG_DIR`, else `~/.claude`.
pub fn config_dir() -> Option<PathBuf> {
    match std::env::var_os("CLAUDE_CONFIG_DIR").filter(|dir| !dir.is_empty()) {
        Some(dir) => Some(PathBuf::from(dir)),
        None => Some(PathBuf::from(std::env::var_os("HOME")?).join(".claude")),
    }
}

/// Where Claude keeps the files: `$CLAUDE_CONFIG_DIR/sessions`, else
/// `~/.claude/sessions`.
pub fn sessions_dir() -> Option<PathBuf> {
    Some(config_dir()?.join("sessions"))
}

/// What a session's file says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub pid: u32,
    /// Claude's session id: it changes with `/clear` and `/resume`.
    pub session: String,
    /// None when the file has no status this core understands.
    pub status: Option<ClaudeStatus>,
}

pub fn read(path: &Path) -> Option<Entry> {
    parse(&std::fs::read_to_string(path).ok()?)
}

fn parse(text: &str) -> Option<Entry> {
    let json: serde_json::Value = serde_json::from_str(text).ok()?;
    let says = match json["status"].as_str() {
        Some("busy") => Some(Says::Busy),
        Some("idle") => Some(Says::Idle),
        Some("waiting") => Some(Says::Waiting),
        _ => None,
    };
    let at = json["statusUpdatedAt"]
        .as_i64()
        .or_else(|| json["updatedAt"].as_i64());
    Some(Entry {
        pid: u32::try_from(json["pid"].as_u64()?).ok()?,
        session: json["sessionId"].as_str()?.to_string(),
        status: says.zip(at).map(|(says, at)| ClaudeStatus {
            says,
            waiting_for: json["waitingFor"].as_str().map(str::to_string),
            at,
        }),
    })
}

/// The file of the process that has this session open.
pub fn find(dir: &Path, session: &str) -> Option<PathBuf> {
    let files = std::fs::read_dir(dir).ok()?;
    files
        .flatten()
        .map(|file| file.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .find(|path| read(path).is_some_and(|entry| entry.session == session))
}

/// Whether a process is still there (also when it's another user's).
pub fn running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // Signal 0 only checks: nothing is sent
    let sent = unsafe { libc::kill(pid, 0) };
    sent == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_session_file_should_say_what_claude_is_doing() {
        let waiting = r#"{"pid":66802,"sessionId":"abc","cwd":"/code","status":"waiting","statusUpdatedAt":1794320759524,"updatedAt":1794320759999,"waitingFor":"permission prompt"}"#;
        let entry = parse(waiting).expect("an entry");
        assert_eq!(entry.pid, 66802);
        assert_eq!(entry.session, "abc");
        assert_eq!(
            entry.status,
            Some(ClaudeStatus {
                says: Says::Waiting,
                waiting_for: Some("permission prompt".into()),
                at: 1_794_320_759_524,
            })
        );
        let busy = r#"{"pid":1,"sessionId":"abc","status":"busy","updatedAt":5}"#;
        let status = parse(busy).expect("an entry").status.expect("a status");
        assert_eq!((status.says, status.at), (Says::Busy, 5));
    }

    #[test]
    fn a_file_without_a_status_it_knows_should_still_name_its_session() {
        let older = r#"{"pid":1,"sessionId":"abc","cwd":"/code"}"#;
        assert_eq!(parse(older).expect("an entry").status, None);
        let newer = r#"{"pid":1,"sessionId":"abc","status":"dreaming","statusUpdatedAt":5}"#;
        assert_eq!(parse(newer).expect("an entry").status, None);
        assert_eq!(parse(r#"{"peerToken":"x"}"#), None);
        assert_eq!(parse("not json"), None);
    }

    #[test]
    fn find_should_pick_the_file_with_the_session() {
        let dir = std::env::temp_dir().join(format!("tui-do-claude-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("folder");
        std::fs::write(dir.join("1.json"), r#"{"pid":1,"sessionId":"one"}"#).expect("write");
        std::fs::write(dir.join("2.json"), r#"{"pid":2,"sessionId":"two"}"#).expect("write");
        std::fs::write(dir.join("2.abc.key"), r#"{"peerToken":"two"}"#).expect("write");
        assert_eq!(find(&dir, "two"), Some(dir.join("2.json")));
        assert_eq!(find(&dir, "three"), None);
        std::fs::remove_dir_all(dir).expect("clean up");
    }

    #[test]
    fn running_should_tell_this_process_from_none() {
        assert!(running(std::process::id()));
        // Far above any pid the system hands out
        assert!(!running(0x3fff_fff0));
    }
}
